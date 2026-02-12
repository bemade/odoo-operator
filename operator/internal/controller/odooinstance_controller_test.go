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

	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/tools/record"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	bemadev1alpha1 "github.com/bemade/odoo-operator/operator/api/v1alpha1"
)

var _ = Describe("OdooInstance Controller", func() {
	var (
		ctx        context.Context
		reconciler *OdooInstanceReconciler
		ns         string
		name       string
	)

	BeforeEach(func() {
		testCounter++
		ctx = context.Background()
		ns = "default"
		name = fmt.Sprintf("odoo-%d", testCounter)
		reconciler = &OdooInstanceReconciler{
			Client:            k8sClient,
			Scheme:            k8sClient.Scheme(),
			Recorder:          record.NewFakeRecorder(100),
			OperatorNamespace: ns,
		}
		// Ensure postgres-clusters secret exists (shared across tests, idempotent).
		secret := &corev1.Secret{
			ObjectMeta: metav1.ObjectMeta{Name: "postgres-clusters", Namespace: ns},
			Data: map[string][]byte{
				"clusters.yaml": []byte("default:\n  host: test-postgres\n  port: 5432\n  adminUser: postgres\n  adminPassword: testpass\n  default: true\n"),
			},
		}
		err := k8sClient.Create(ctx, secret)
		if err != nil && !apierrors.IsAlreadyExists(err) {
			Expect(err).NotTo(HaveOccurred())
		}
	})

	reconcileInstance := func() (reconcile.Result, error) {
		return reconciler.Reconcile(ctx, reconcile.Request{
			NamespacedName: types.NamespacedName{Name: name, Namespace: ns},
		})
	}

	getInstance := func() *bemadev1alpha1.OdooInstance {
		obj := &bemadev1alpha1.OdooInstance{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, obj)).To(Succeed())
		return obj
	}

	createInstance := func(mutators ...func(*bemadev1alpha1.OdooInstance)) *bemadev1alpha1.OdooInstance {
		obj := &bemadev1alpha1.OdooInstance{
			ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: ns},
			Spec: bemadev1alpha1.OdooInstanceSpec{
				Image:         "odoo:18.0",
				AdminPassword: "admin",
				Replicas:      2,
				Ingress: bemadev1alpha1.IngressSpec{
					Hosts:  []string{"test.example.com"},
					Issuer: "letsencrypt",
				},
			},
		}
		for _, fn := range mutators {
			fn(obj)
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		return obj
	}

	getDeployment := func() *appsv1.Deployment {
		dep := &appsv1.Deployment{}
		Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, dep)).To(Succeed())
		return dep
	}

	simulateDeploymentReady := func(readyReplicas int32) {
		dep := getDeployment()
		patch := client.MergeFrom(dep.DeepCopy())
		dep.Status.ReadyReplicas = readyReplicas
		dep.Status.Replicas = readyReplicas
		Expect(k8sClient.Status().Patch(ctx, dep, patch)).To(Succeed())
	}

	markDBInitialized := func() {
		jobName := fmt.Sprintf("init-%d", testCounter)
		job := &bemadev1alpha1.OdooInitJob{
			ObjectMeta: metav1.ObjectMeta{Name: jobName, Namespace: ns},
			Spec: bemadev1alpha1.OdooInitJobSpec{
				OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: name},
			},
		}
		Expect(k8sClient.Create(ctx, job)).To(Succeed())
		patch := client.MergeFrom(job.DeepCopy())
		job.Status.Phase = bemadev1alpha1.PhaseCompleted
		Expect(k8sClient.Status().Patch(ctx, job, patch)).To(Succeed())
	}

	createUpgradeJob := func(phase bemadev1alpha1.Phase) *bemadev1alpha1.OdooUpgradeJob {
		jobName := fmt.Sprintf("upgrade-%d", testCounter)
		obj := &bemadev1alpha1.OdooUpgradeJob{
			ObjectMeta: metav1.ObjectMeta{Name: jobName, Namespace: ns},
			Spec:       bemadev1alpha1.OdooUpgradeJobSpec{OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: name}},
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		patch := client.MergeFrom(obj.DeepCopy())
		obj.Status.Phase = phase
		Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())
		return obj
	}

	createRestoreJob := func(phase bemadev1alpha1.Phase) *bemadev1alpha1.OdooRestoreJob {
		jobName := fmt.Sprintf("rjob-%d", testCounter)
		obj := &bemadev1alpha1.OdooRestoreJob{
			ObjectMeta: metav1.ObjectMeta{Name: jobName, Namespace: ns},
			Spec: bemadev1alpha1.OdooRestoreJobSpec{
				OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: name},
				Source:          bemadev1alpha1.RestoreSource{Type: bemadev1alpha1.RestoreSourceTypeS3},
			},
		}
		Expect(k8sClient.Create(ctx, obj)).To(Succeed())
		patch := client.MergeFrom(obj.DeepCopy())
		obj.Status.Phase = phase
		Expect(k8sClient.Status().Patch(ctx, obj, patch)).To(Succeed())
		return obj
	}

	Describe("child resource creation", func() {
		It("creates all child resources on first reconcile", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-filestore-pvc", Namespace: ns}, &corev1.PersistentVolumeClaim{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-user", Namespace: ns}, &corev1.Secret{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-db-config", Namespace: ns}, &corev1.Secret{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-conf", Namespace: ns}, &corev1.ConfigMap{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, &corev1.Service{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, &networkingv1.Ingress{})).To(Succeed())
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, &appsv1.Deployment{})).To(Succeed())
		})

		It("sets owner references on all child resources", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			dep := getDeployment()
			Expect(dep.OwnerReferences).To(HaveLen(1))
			Expect(dep.OwnerReferences[0].Name).To(Equal(name))
		})

		It("is idempotent on repeated reconciles", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
		})

		It("sets the db-config secret with host and port from the postgres cluster", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			secret := &corev1.Secret{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-db-config", Namespace: ns}, secret)).To(Succeed())
			Expect(string(secret.Data["host"])).To(Equal("test-postgres"))
			Expect(string(secret.Data["port"])).To(Equal("5432"))
		})

		It("sets the odoo-user secret with the correct username format", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			secret := &corev1.Secret{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-user", Namespace: ns}, secret)).To(Succeed())
			Expect(string(secret.Data["username"])).To(Equal(fmt.Sprintf("odoo.%s.%s", ns, name)))
			Expect(secret.Data["password"]).NotTo(BeEmpty())
		})

		It("does not change the odoo-user password on subsequent reconciles", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			secret := &corev1.Secret{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-user", Namespace: ns}, secret)).To(Succeed())
			originalPassword := string(secret.Data["password"])

			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-user", Namespace: ns}, secret)).To(Succeed())
			Expect(string(secret.Data["password"])).To(Equal(originalPassword))
		})

		It("writes odoo.conf with db_user and list_db=False", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			cm := &corev1.ConfigMap{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name + "-odoo-conf", Namespace: ns}, cm)).To(Succeed())
			conf := cm.Data["odoo.conf"]
			Expect(conf).To(ContainSubstring("list_db = False"))
			Expect(conf).To(ContainSubstring(fmt.Sprintf("db_user = odoo.%s.%s", ns, name)))
		})

		It("creates ingress with cert-manager annotation and TLS block", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			ing := &networkingv1.Ingress{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, ing)).To(Succeed())
			Expect(ing.Annotations["cert-manager.io/cluster-issuer"]).To(Equal("letsencrypt"))
			Expect(ing.Spec.TLS).To(HaveLen(1))
			Expect(ing.Spec.TLS[0].Hosts).To(ContainElement("test.example.com"))
			Expect(ing.Spec.TLS[0].SecretName).To(Equal(name + "-tls"))
		})

		It("creates ingress rules for / and /websocket", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			ing := &networkingv1.Ingress{}
			Expect(k8sClient.Get(ctx, types.NamespacedName{Name: name, Namespace: ns}, ing)).To(Succeed())
			Expect(ing.Spec.Rules).To(HaveLen(1))
			paths := ing.Spec.Rules[0].HTTP.Paths
			Expect(paths).To(HaveLen(2))
			Expect(paths[0].Path).To(Equal("/websocket"))
			Expect(paths[1].Path).To(Equal("/"))
		})
	})

	Describe("Uninitialized phase", func() {
		It("sets phase to Uninitialized when no InitJob has ever completed", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseUninitialized))
		})

		It("holds Deployment at 0 replicas when Uninitialized", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(*getDeployment().Spec.Replicas).To(Equal(int32(0)))
		})

		It("sets Ready=False condition when not Running", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			instance := getInstance()
			cond := findCondition(instance.Status.Conditions, "Ready")
			Expect(cond).NotTo(BeNil())
			Expect(string(cond.Status)).To(Equal("False"))
			Expect(cond.Reason).To(Equal("Uninitialized"))
		})
	})

	Describe("Initializing phase", func() {
		It("sets phase to Initializing when an OdooInitJob is Running", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			jobName := fmt.Sprintf("init-running-%d", testCounter)
			job := &bemadev1alpha1.OdooInitJob{
				ObjectMeta: metav1.ObjectMeta{Name: jobName, Namespace: ns},
				Spec: bemadev1alpha1.OdooInitJobSpec{
					OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: name},
				},
			}
			Expect(k8sClient.Create(ctx, job)).To(Succeed())
			patch := client.MergeFrom(job.DeepCopy())
			job.Status.Phase = bemadev1alpha1.PhaseRunning
			Expect(k8sClient.Status().Patch(ctx, job, patch)).To(Succeed())

			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseInitializing))
		})
	})

	Describe("InitFailed phase", func() {
		It("sets phase to InitFailed when an OdooInitJob has Failed", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			jobName := fmt.Sprintf("init-failed-%d", testCounter)
			job := &bemadev1alpha1.OdooInitJob{
				ObjectMeta: metav1.ObjectMeta{Name: jobName, Namespace: ns},
				Spec: bemadev1alpha1.OdooInitJobSpec{
					OdooInstanceRef: bemadev1alpha1.OdooInstanceRef{Name: name},
				},
			}
			Expect(k8sClient.Create(ctx, job)).To(Succeed())
			patch := client.MergeFrom(job.DeepCopy())
			job.Status.Phase = bemadev1alpha1.PhaseFailed
			Expect(k8sClient.Status().Patch(ctx, job, patch)).To(Succeed())

			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseInitFailed))
		})
	})

	Describe("after initialization", func() {
		BeforeEach(func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			markDBInitialized()
		})

		It("sets status.dbInitialized=true when a completed OdooInitJob is observed", func() {
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.DBInitialized).To(BeTrue())
		})

		It("scales Deployment to spec.replicas after initialization", func() {
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(*getDeployment().Spec.Replicas).To(Equal(int32(2)))
		})

		Describe("Starting phase", func() {
			It("is Starting when Deployment has 0 ready replicas", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseStarting))
			})
		})

		Describe("Running phase", func() {
			It("is Running when readyReplicas == spec.replicas", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				simulateDeploymentReady(2)

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseRunning))
			})

			It("sets Ready=True condition when Running", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				simulateDeploymentReady(2)

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				instance := getInstance()
				Expect(instance.Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseRunning))
				cond := findCondition(instance.Status.Conditions, "Ready")
				Expect(cond).NotTo(BeNil())
				Expect(string(cond.Status)).To(Equal("True"))
				Expect(cond.Reason).To(Equal("Running"))
			})

		It("sets status.url from the first ingress host", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				simulateDeploymentReady(2)

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.URL).To(Equal("https://test.example.com"))
			})

			It("syncs status.readyReplicas from the Deployment", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				simulateDeploymentReady(2)

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.ReadyReplicas).To(Equal(int32(2)))
			})
		})

		Describe("Degraded phase", func() {
			It("is Degraded when 0 < readyReplicas < spec.replicas", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				simulateDeploymentReady(1)

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseDegraded))
			})
		})

		Describe("Stopped phase", func() {
			It("is Stopped when spec.replicas == 0", func() {
				inst := getInstance()
				patch := client.MergeFrom(inst.DeepCopy())
				inst.Spec.Replicas = 0
				Expect(k8sClient.Patch(ctx, inst, patch)).To(Succeed())

				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseStopped))
			})

			It("scales Deployment to 0 when Stopped", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())

				inst := getInstance()
				patch := client.MergeFrom(inst.DeepCopy())
				inst.Spec.Replicas = 0
				Expect(k8sClient.Patch(ctx, inst, patch)).To(Succeed())

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(*getDeployment().Spec.Replicas).To(Equal(int32(0)))
			})
		})

		Describe("drift correction", func() {
			It("restores Deployment replicas when changed externally", func() {
				_, err := reconcileInstance()
				Expect(err).NotTo(HaveOccurred())

				dep := getDeployment()
				patch := client.MergeFrom(dep.DeepCopy())
				five := int32(5)
				dep.Spec.Replicas = &five
				Expect(k8sClient.Patch(ctx, dep, patch)).To(Succeed())

				_, err = reconcileInstance()
				Expect(err).NotTo(HaveOccurred())
				Expect(*getDeployment().Spec.Replicas).To(Equal(int32(2)))
			})
		})
	})

	Describe("job-driven phases", func() {
		BeforeEach(func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			markDBInitialized()
			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
		})

		It("is Upgrading when an OdooUpgradeJob is Running", func() {
			createUpgradeJob(bemadev1alpha1.PhaseRunning)
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseUpgrading))
		})

		It("is UpgradeFailed when an OdooUpgradeJob has Failed", func() {
			createUpgradeJob(bemadev1alpha1.PhaseFailed)
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseUpgradeFailed))
		})

		It("is Restoring when an OdooRestoreJob is Running", func() {
			createRestoreJob(bemadev1alpha1.PhaseRunning)
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseRestoring))
		})

		It("is RestoreFailed when an OdooRestoreJob has Failed", func() {
			createRestoreJob(bemadev1alpha1.PhaseFailed)
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			Expect(getInstance().Status.Phase).To(Equal(bemadev1alpha1.OdooInstancePhaseRestoreFailed))
		})
	})

	Describe("event publishing", func() {
		// drainEvents returns all events currently buffered in the fake recorder.
		drainEvents := func() []string {
			ch := reconciler.Recorder.(*record.FakeRecorder).Events
			var events []string
			for {
				select {
				case e := <-ch:
					events = append(events, e)
				default:
					return events
				}
			}
		}

		It("emits DefaultsApplied on first reconcile when spec fields are missing", func() {
			// Create instance without image/filestore/database so applyDefaults fires.
			obj := &bemadev1alpha1.OdooInstance{
				ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: ns},
				Spec: bemadev1alpha1.OdooInstanceSpec{
					AdminPassword: "admin",
					Replicas:      1,
					Ingress:       bemadev1alpha1.IngressSpec{Hosts: []string{"test.example.com"}},
				},
			}
			Expect(k8sClient.Create(ctx, obj)).To(Succeed())
			_, err := reconciler.Reconcile(ctx, reconcile.Request{
				NamespacedName: types.NamespacedName{Name: name, Namespace: ns},
			})
			Expect(err).NotTo(HaveOccurred())

			events := drainEvents()
			Expect(events).To(ContainElement(ContainSubstring("DefaultsApplied")))
		})

		It("emits DefaultsApplied for database cluster on first reconcile", func() {
			// Create instance without database.cluster set.
			obj := &bemadev1alpha1.OdooInstance{
				ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: ns},
				Spec: bemadev1alpha1.OdooInstanceSpec{
					Image:         "odoo:18.0",
					AdminPassword: "admin",
					Replicas:      1,
					Ingress:       bemadev1alpha1.IngressSpec{Hosts: []string{"test.example.com"}},
				},
			}
			Expect(k8sClient.Create(ctx, obj)).To(Succeed())
			_, err := reconciler.Reconcile(ctx, reconcile.Request{
				NamespacedName: types.NamespacedName{Name: name, Namespace: ns},
			})
			Expect(err).NotTo(HaveOccurred())

			events := drainEvents()
			Expect(events).To(ContainElement(ContainSubstring("DefaultsApplied")))
			Expect(events).To(ContainElement(ContainSubstring("default"))) // resolved cluster name
		})

		It("emits PhaseChanged when phase transitions", func() {
			createInstance()
			// First reconcile: no InitJob → Uninitialized phase (from empty "").
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			drainEvents() // discard defaults events

			// Simulate DB initialized so phase can advance.
			instance := getInstance()
			patch := client.MergeFrom(instance.DeepCopy())
			instance.Status.DBInitialized = true
			Expect(k8sClient.Status().Patch(ctx, instance, patch)).To(Succeed())

			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			events := drainEvents()
			Expect(events).To(ContainElement(ContainSubstring("PhaseChanged")))
		})

		It("does not emit PhaseChanged when phase is unchanged", func() {
			createInstance()
			_, err := reconcileInstance()
			Expect(err).NotTo(HaveOccurred())
			drainEvents() // discard first-reconcile events

			// Second reconcile — phase stays Uninitialized.
			_, err = reconcileInstance()
			Expect(err).NotTo(HaveOccurred())

			events := drainEvents()
			Expect(events).NotTo(ContainElement(ContainSubstring("PhaseChanged")))
		})
	})
})

// findCondition returns the condition with the given type, or nil if not found.
func findCondition(conditions []metav1.Condition, condType string) *metav1.Condition {
	for i := range conditions {
		if conditions[i].Type == condType {
			return &conditions[i]
		}
	}
	return nil
}
