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
	"io"
	"net/http"
	"net/http/httptest"
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

// testCounter provides unique resource names across tests in the same suite run.
var testCounter int

var _ = Describe("OdooInitJob Controller", func() {
	var (
		ctx        context.Context
		reconciler *OdooInitJobReconciler
		ns         string
		iName      string // OdooInstance name for this test
		jName      string // OdooInitJob name for this test
	)

	BeforeEach(func() {
		testCounter++
		ctx = context.Background()
		ns = "default"
		iName = fmt.Sprintf("test-instance-%d", testCounter)
		jName = fmt.Sprintf("test-initjob-%d", testCounter)
		reconciler = &OdooInitJobReconciler{
			Client: k8sClient,
			Scheme: k8sClient.Scheme(),
		}
	})

	// --- helpers ---

	reconcileJob := func() (reconcile.Result, error) {
		return reconciler.Reconcile(ctx, reconcile.Request{
			NamespacedName: types.NamespacedName{Name: jName, Namespace: ns},
		})
	}

	getInitJob := func() *bemadev1alpha1.OdooInitJob {
		obj := &bemadev1alpha1.OdooInitJob{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: jName, Namespace: ns}, obj)).To(Succeed())
		return obj
	}

	createOdooInstance := func(replicas int32) *bemadev1alpha1.OdooInstance {
		obj := &bemadev1alpha1.OdooInstance{
			ObjectMeta: metav1.ObjectMeta{Name: iName, Namespace: ns},
			Spec: bemadev1alpha1.OdooInstanceSpec{
				Image:         "odoo:18.0",
				AdminPassword: "admin",
				Replicas:      replicas,
				Ingress: bemadev1alpha1.IngressSpec{
					Hosts:  []string{"test.example.com"},
					Issuer: "letsencrypt",
				},
			},
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	createInitJob := func(mods []string, mutators ...func(*bemadev1alpha1.OdooInitJob)) *bemadev1alpha1.OdooInitJob {
		obj := &bemadev1alpha1.OdooInitJob{
			ObjectMeta: metav1.ObjectMeta{Name: jName, Namespace: ns},
			Spec: bemadev1alpha1.OdooInitJobSpec{
				OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: iName},
				Modules:         mods,
			},
		}
		for _, fn := range mutators {
			fn(obj)
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	setInitJobStatus := func(phase bemadev1alpha1.Phase) {
		obj := getInitJob()
		patch := client.MergeFrom(obj.DeepCopy())
		obj.Status.Phase = phase
		Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())
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

	// --- tests ---

	Describe("terminal state short-circuit", func() {
		It("does nothing when phase is already Completed", func() {
			createInitJob([]string{"base"})
			setInitJobStatus(bemadev1alpha1.PhaseCompleted)

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result).To(Equal(reconcile.Result{}))

			// No child Job should have been created
			jobs := &batchv1.JobList{}
			Expect(k8sClient.List(ctx, jobs, client.InNamespace(ns),
				client.MatchingLabels{"app": jName})).To(Succeed())
			Expect(jobs.Items).To(BeEmpty())
		})

		It("does nothing when phase is already Failed", func() {
			createInitJob([]string{"base"})
			setInitJobStatus(bemadev1alpha1.PhaseFailed)

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result).To(Equal(reconcile.Result{}))
		})
	})

	Describe("when the referenced OdooInstance does not exist", func() {
		It("sets status to Failed with a descriptive message", func() {
			createInitJob([]string{"base"})

			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getInitJob()
			Expect(updated.Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
			Expect(updated.Status.Message).To(ContainSubstring(iName))
		})
	})

	Describe("fresh OdooInitJob with an existing OdooInstance", func() {
		BeforeEach(func() {
			createOdooInstance(2)
		})

		It("creates a child Job and sets status to Running", func() {
			createInitJob([]string{"base"})

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result.RequeueAfter).To(Equal(10 * time.Second))

			updated := getInitJob()
			Expect(updated.Status.Phase).To(Equal(bemadev1alpha1.PhaseRunning))
			Expect(updated.Status.JobName).NotTo(BeEmpty())
			Expect(updated.Status.StartTime).NotTo(BeNil())

			// The child Job must exist in the cluster
			_ = getChildJob(updated.Status.JobName)
		})

		It("uses the image from the OdooInstance spec", func() {
			createInitJob([]string{"base"})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getInitJob()
			job := getChildJob(updated.Status.JobName)
			Expect(job.Spec.Template.Spec.Containers[0].Image).To(Equal("odoo:18.0"))
		})

		It("passes modules as a comma-separated -i argument", func() {
			createInitJob([]string{"sale", "purchase", "stock"})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getInitJob()
			job := getChildJob(updated.Status.JobName)
			args := job.Spec.Template.Spec.Containers[0].Args
			Expect(args).To(ContainElement("sale,purchase,stock"))
		})

		It("defaults to [base] when no modules are specified", func() {
			createInitJob(nil)
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getInitJob()
			job := getChildJob(updated.Status.JobName)
			args := job.Spec.Template.Spec.Containers[0].Args
			Expect(args).To(ContainElement("base"))
		})

		It("sets the OdooInitJob as owner of the child Job", func() {
			createInitJob([]string{"base"})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getInitJob()
			job := getChildJob(updated.Status.JobName)
			Expect(job.OwnerReferences).To(HaveLen(1))
			Expect(job.OwnerReferences[0].Name).To(Equal(jName))
			Expect(job.OwnerReferences[0].Kind).To(Equal("OdooInitJob"))
		})

		It("is idempotent — second reconcile while Running does not create another Job", func() {
			createInitJob([]string{"base"})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			firstJob := getInitJob().Status.JobName

			// Reconcile again without the job completing
			_, err = reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			Expect(getInitJob().Status.JobName).To(Equal(firstJob))
		})

		Context("when the child Job succeeds", func() {
			It("sets status to Completed with a CompletionTime", func() {
				createInitJob([]string{"base"})
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getInitJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getInitJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseCompleted))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})

		Context("when the child Job fails", func() {
			It("sets status to Failed with a CompletionTime", func() {
				createInitJob([]string{"base"})
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getInitJob().Status.JobName
				simulateJobResult(jobName, 0, 1)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getInitJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})

		Context("when a webhook is configured", func() {
			var (
				server          *httptest.Server
				receivedBody    []byte
				receivedHeaders http.Header
			)

			BeforeEach(func() {
				server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					receivedHeaders = r.Header
					receivedBody, _ = io.ReadAll(r.Body)
					w.WriteHeader(http.StatusOK)
				}))
				reconciler.HTTPClient = server.Client()
			})

			AfterEach(func() {
				server.Close()
			})

			It("POSTs to the webhook URL on completion", func() {
				createInitJob([]string{"base"}, func(j *bemadev1alpha1.OdooInitJob) {
					j.Spec.Webhook = &bemadev1alpha1.WebhookConfig{URL: server.URL}
				})

				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getInitJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				Expect(receivedHeaders.Get("Content-Type")).To(Equal("application/json"))
				Expect(receivedBody).To(ContainSubstring(jName))
				Expect(receivedBody).To(ContainSubstring(`"phase":"Completed"`))
			})

			It("includes a Bearer token when configured", func() {
				createInitJob([]string{"base"}, func(j *bemadev1alpha1.OdooInitJob) {
					j.Spec.Webhook = &bemadev1alpha1.WebhookConfig{
						URL:   server.URL,
						Token: "my-secret-token",
					}
				})

				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getInitJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				Expect(receivedHeaders.Get("Authorization")).To(Equal("Bearer my-secret-token"))
			})
		})
	})
})
