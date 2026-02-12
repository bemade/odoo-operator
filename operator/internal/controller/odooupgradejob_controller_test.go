/*
Copyright 2026 Marc Durepos, Bemade Inc.

This file is part of odoo-operator.

odoo-operator is free software: you can redistribute it and/or modify it under
the terms of the GNU Lesser General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version.

odoo-operator is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU Lesser General Public License for more
details.

You should have received a copy of the GNU Lesser General Public License along
with odoo-operator. If not, see <https://www.gnu.org/licenses/>.
*/

package controller

import (
	"context"
	"fmt"
	"time"

	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	batchv1 "k8s.io/api/batch/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	bemadev1alpha1 "github.com/bemade/odoo-operator/operator/api/v1alpha1"
)

var _ = Describe("OdooUpgradeJob Controller", func() {
	var (
		ctx        context.Context
		reconciler *OdooUpgradeJobReconciler
		ns         string
		iName      string
		jName      string
	)

	BeforeEach(func() {
		testCounter++
		ctx = context.Background()
		ns = "default"
		iName = fmt.Sprintf("upgrade-instance-%d", testCounter)
		jName = fmt.Sprintf("upgrade-job-%d", testCounter)
		reconciler = &OdooUpgradeJobReconciler{
			Client: k8sClient,
			Scheme: k8sClient.Scheme(),
		}
	})

	reconcileJob := func() (reconcile.Result, error) {
		return reconciler.Reconcile(ctx, reconcile.Request{
			NamespacedName: types.NamespacedName{Name: jName, Namespace: ns},
		})
	}

	getUpgradeJob := func() *bemadev1alpha1.OdooUpgradeJob {
		obj := &bemadev1alpha1.OdooUpgradeJob{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: jName, Namespace: ns}, obj)).To(Succeed())
		return obj
	}

	createOdooInstance := func() *bemadev1alpha1.OdooInstance {
		obj := &bemadev1alpha1.OdooInstance{
			ObjectMeta: metav1.ObjectMeta{Name: iName, Namespace: ns},
			Spec: bemadev1alpha1.OdooInstanceSpec{
				Image:         "odoo:18.0",
				AdminPassword: "admin",
				Replicas:      2,
				Ingress:       bemadev1alpha1.IngressSpec{Hosts: []string{"test.example.com"}, Issuer: "letsencrypt"},
			},
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	createUpgradeJob := func(mutators ...func(*bemadev1alpha1.OdooUpgradeJob)) *bemadev1alpha1.OdooUpgradeJob {
		obj := &bemadev1alpha1.OdooUpgradeJob{
			ObjectMeta: metav1.ObjectMeta{Name: jName, Namespace: ns},
			Spec: bemadev1alpha1.OdooUpgradeJobSpec{
				OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: iName},
			},
		}
		for _, fn := range mutators {
			fn(obj)
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	getChildJob := func(jobName string) *batchv1.Job {
		job := &batchv1.Job{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: jobName, Namespace: ns}, job)).To(Succeed())
		return job
	}

	simulateJobResult := func(jobName string, succeeded, failed int32) {
		job := getChildJob(jobName)
		patch := client.MergeFrom(job.DeepCopy())
		job.Status.Succeeded = succeeded
		job.Status.Failed = failed
		Expect(k8sClient.Status().Patch(ctx, job, patch)).To(Succeed())
	}

	Describe("terminal state short-circuit", func() {
		It("does nothing when phase is already Completed", func() {
			createUpgradeJob()
			obj := getUpgradeJob()
			patch := client.MergeFrom(obj.DeepCopy())
			obj.Status.Phase = bemadev1alpha1.PhaseCompleted
			Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result).To(Equal(reconcile.Result{}))
		})
	})

	Describe("scheduled upgrade", func() {
		It("requeues when scheduledTime is in the future", func() {
			createUpgradeJob(func(j *bemadev1alpha1.OdooUpgradeJob) {
				future := metav1.NewTime(time.Now().Add(1 * time.Hour))
				j.Spec.ScheduledTime = &future
			})

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result.RequeueAfter).To(BeNumerically(">", 59*time.Minute))
			// No job should have been created yet
			Expect(getUpgradeJob().Status.JobName).To(BeEmpty())
		})
	})

	Describe("when the referenced OdooInstance does not exist", func() {
		It("sets status to Failed", func() {
			createUpgradeJob()
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			Expect(getUpgradeJob().Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
			Expect(getUpgradeJob().Status.Message).To(ContainSubstring(iName))
		})
	})

	Describe("fresh OdooUpgradeJob with an existing OdooInstance", func() {
		BeforeEach(func() {
			createOdooInstance()
		})

		It("creates a child Job and sets status to Running", func() {
			createUpgradeJob()

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result.RequeueAfter).To(Equal(10 * time.Second))

			updated := getUpgradeJob()
			Expect(updated.Status.Phase).To(Equal(bemadev1alpha1.PhaseRunning))
			Expect(updated.Status.JobName).NotTo(BeEmpty())
			Expect(updated.Status.StartTime).NotTo(BeNil())
			Expect(updated.Status.OriginalReplicas).To(Equal(int32(2)))

			_ = getChildJob(updated.Status.JobName)
		})

		It("defaults to upgrading all modules when none specified", func() {
			createUpgradeJob()
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getUpgradeJob()
			job := getChildJob(updated.Status.JobName)
			args := job.Spec.Template.Spec.Containers[0].Args
			Expect(args).To(ContainElement("all"))
		})

		It("passes -u flag for modules to upgrade", func() {
			createUpgradeJob(func(j *bemadev1alpha1.OdooUpgradeJob) {
				j.Spec.Modules = []string{"sale", "purchase"}
			})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getUpgradeJob()
			job := getChildJob(updated.Status.JobName)
			args := job.Spec.Template.Spec.Containers[0].Args
			Expect(args).To(ContainElement("-u"))
			Expect(args).To(ContainElement("sale,purchase"))
		})

		It("passes -i flag for modules to install", func() {
			createUpgradeJob(func(j *bemadev1alpha1.OdooUpgradeJob) {
				j.Spec.ModulesInstall = []string{"helpdesk"}
			})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getUpgradeJob()
			job := getChildJob(updated.Status.JobName)
			args := job.Spec.Template.Spec.Containers[0].Args
			Expect(args).To(ContainElement("-i"))
			Expect(args).To(ContainElement("helpdesk"))
		})

		Context("when the child Job succeeds", func() {
			It("sets status to Completed and records CompletionTime", func() {
				createUpgradeJob()
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getUpgradeJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getUpgradeJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseCompleted))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})

		Context("when the child Job fails", func() {
			It("sets status to Failed and records CompletionTime", func() {
				createUpgradeJob()
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getUpgradeJob().Status.JobName
				simulateJobResult(jobName, 0, 1)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getUpgradeJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})
	})
})
