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

var _ = Describe("OdooRestoreJob Controller", func() {
	var (
		ctx        context.Context
		reconciler *OdooRestoreJobReconciler
		ns         string
		iName      string
		jName      string
	)

	BeforeEach(func() {
		testCounter++
		ctx = context.Background()
		ns = "default"
		iName = fmt.Sprintf("restore-instance-%d", testCounter)
		jName = fmt.Sprintf("restore-job-%d", testCounter)
		reconciler = &OdooRestoreJobReconciler{
			Client: k8sClient,
			Scheme: k8sClient.Scheme(),
		}
	})

	reconcileJob := func() (reconcile.Result, error) {
		return reconciler.Reconcile(ctx, reconcile.Request{
			NamespacedName: types.NamespacedName{Name: jName, Namespace: ns},
		})
	}

	getRestoreJob := func() *bemadev1alpha1.OdooRestoreJob {
		obj := &bemadev1alpha1.OdooRestoreJob{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: jName, Namespace: ns}, obj)).To(Succeed())
		return obj
	}

	createOdooInstance := func() *bemadev1alpha1.OdooInstance {
		obj := &bemadev1alpha1.OdooInstance{
			ObjectMeta: metav1.ObjectMeta{Name: iName, Namespace: ns},
			Spec: bemadev1alpha1.OdooInstanceSpec{
				Image:         "odoo:18.0",
				AdminPassword: "admin",
				Replicas:      1,
				Ingress:       bemadev1alpha1.IngressSpec{Hosts: []string{"test.example.com"}, Issuer: "letsencrypt"},
			},
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	createRestoreJob := func(mutators ...func(*bemadev1alpha1.OdooRestoreJob)) *bemadev1alpha1.OdooRestoreJob {
		obj := &bemadev1alpha1.OdooRestoreJob{
			ObjectMeta: metav1.ObjectMeta{Name: jName, Namespace: ns},
			Spec: bemadev1alpha1.OdooRestoreJobSpec{
				OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: iName},
				Source: bemadev1alpha1.RestoreSource{
					Type: bemadev1alpha1.RestoreSourceTypeS3,
					S3: &bemadev1alpha1.S3Config{
						Bucket:    "test-bucket",
						ObjectKey: "backups/test.zip",
						Endpoint:  "https://s3.example.com",
					},
				},
				Format: bemadev1alpha1.BackupFormatZip,
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
			createRestoreJob()
			obj := getRestoreJob()
			patch := client.MergeFrom(obj.DeepCopy())
			obj.Status.Phase = bemadev1alpha1.PhaseCompleted
			Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result).To(Equal(reconcile.Result{}))
		})

		It("does nothing when phase is already Failed", func() {
			createRestoreJob()
			obj := getRestoreJob()
			patch := client.MergeFrom(obj.DeepCopy())
			obj.Status.Phase = bemadev1alpha1.PhaseFailed
			Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result).To(Equal(reconcile.Result{}))
		})
	})

	Describe("when the referenced OdooInstance does not exist", func() {
		It("sets status to Failed", func() {
			createRestoreJob()
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			Expect(getRestoreJob().Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
			Expect(getRestoreJob().Status.Message).To(ContainSubstring(iName))
		})
	})

	Describe("fresh OdooRestoreJob with an existing OdooInstance", func() {
		BeforeEach(func() {
			createOdooInstance()
		})

		It("creates a child Job and sets status to Running", func() {
			createRestoreJob()

			result, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())
			Expect(result.RequeueAfter).To(Equal(10 * time.Second))

			updated := getRestoreJob()
			Expect(updated.Status.Phase).To(Equal(bemadev1alpha1.PhaseRunning))
			Expect(updated.Status.JobName).NotTo(BeEmpty())
			Expect(updated.Status.StartTime).NotTo(BeNil())
		})

		It("uses a downloader init container and a restore init container for S3 source", func() {
			createRestoreJob()
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getRestoreJob()
			job := getChildJob(updated.Status.JobName)
			Expect(job.Spec.Template.Spec.InitContainers).To(HaveLen(2))
			Expect(job.Spec.Template.Spec.InitContainers[0].Name).To(Equal("downloader"))
			Expect(job.Spec.Template.Spec.InitContainers[1].Name).To(Equal("restore"))
			// Sentinel main container
			Expect(job.Spec.Template.Spec.Containers[0].Name).To(Equal("done"))
		})

		It("uses only a restore init container when source type is not S3", func() {
			createRestoreJob(func(j *bemadev1alpha1.OdooRestoreJob) {
				j.Spec.Source = bemadev1alpha1.RestoreSource{
					Type: bemadev1alpha1.RestoreSourceTypeOdoo,
					Odoo: &bemadev1alpha1.OdooLiveSource{
						URL: "https://source.example.com",
					},
				}
			})
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getRestoreJob()
			job := getChildJob(updated.Status.JobName)
			// Only the restore init container — no downloader for Odoo live source
			Expect(job.Spec.Template.Spec.InitContainers).To(HaveLen(1))
			Expect(job.Spec.Template.Spec.InitContainers[0].Name).To(Equal("restore"))
		})

		It("sets the OdooRestoreJob as owner of the child Job", func() {
			createRestoreJob()
			_, err := reconcileJob()
			Expect(err).NotTo(HaveOccurred())

			updated := getRestoreJob()
			job := getChildJob(updated.Status.JobName)
			Expect(job.OwnerReferences).To(HaveLen(1))
			Expect(job.OwnerReferences[0].Name).To(Equal(jName))
		})

		Context("when the child Job succeeds", func() {
			It("sets status to Completed", func() {
				createRestoreJob()
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getRestoreJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getRestoreJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseCompleted))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})

		Context("when the child Job fails", func() {
			It("sets status to Failed", func() {
				createRestoreJob()
				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getRestoreJob().Status.JobName
				simulateJobResult(jobName, 0, 1)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				final := getRestoreJob()
				Expect(final.Status.Phase).To(Equal(bemadev1alpha1.PhaseFailed))
				Expect(final.Status.CompletionTime).NotTo(BeNil())
			})
		})

		Context("when a webhook is configured", func() {
			var (
				server       *httptest.Server
				receivedBody []byte
			)

			BeforeEach(func() {
				server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					receivedBody, _ = io.ReadAll(r.Body)
					w.WriteHeader(http.StatusOK)
				}))
				reconciler.HTTPClient = server.Client()
			})

			AfterEach(func() { server.Close() })

			It("POSTs to the webhook URL on completion", func() {
				createRestoreJob(func(j *bemadev1alpha1.OdooRestoreJob) {
					j.Spec.Webhook = &bemadev1alpha1.WebhookConfig{URL: server.URL}
				})

				_, err := reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				jobName := getRestoreJob().Status.JobName
				simulateJobResult(jobName, 1, 0)

				_, err = reconcileJob()
				Expect(err).NotTo(HaveOccurred())

				Expect(receivedBody).To(ContainSubstring(jName))
				Expect(receivedBody).To(ContainSubstring(`"phase":"Completed"`))
			})
		})
	})
})
