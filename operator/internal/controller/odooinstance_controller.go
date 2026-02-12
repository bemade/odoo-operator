/*
Copyright 2026 Bemade Inc..

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

package controller

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"strings"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	logf "sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"
	sigsyaml "sigs.k8s.io/yaml"

	bemadev1alpha1 "github.com/bemade/odoo-operator/operator/api/v1alpha1"
)

// postgresClusterConfig is the per-cluster entry from the postgres-clusters Secret.
type postgresClusterConfig struct {
	Host    string `json:"host"`
	Port    int    `json:"port"`
	Default bool   `json:"default"`
}

// OdooInstanceReconciler reconciles a OdooInstance object.
type OdooInstanceReconciler struct {
	client.Client
	Scheme                 *runtime.Scheme
	OperatorNamespace      string
	PostgresClustersSecret string
	Defaults               OperatorDefaults
}

// applyDefaults writes operator-level defaults into any unset spec fields and
// returns true if the spec was changed. The caller must persist the change and
// requeue; downstream reconcile logic can then assume all fields are populated.
func (r *OdooInstanceReconciler) applyDefaults(instance *bemadev1alpha1.OdooInstance) bool {
	changed := false

	if instance.Spec.Image == "" {
		img := r.Defaults.OdooImage
		if img == "" {
			img = "odoo:18.0"
		}
		instance.Spec.Image = img
		changed = true
	}

	if instance.Spec.Filestore == nil {
		instance.Spec.Filestore = &bemadev1alpha1.FilestoreSpec{}
	}
	if instance.Spec.Filestore.StorageClass == "" {
		sc := r.Defaults.StorageClass
		if sc == "" {
			sc = "standard"
		}
		instance.Spec.Filestore.StorageClass = sc
		changed = true
	}
	if instance.Spec.Filestore.StorageSize == "" {
		sz := r.Defaults.StorageSize
		if sz == "" {
			sz = "2Gi"
		}
		instance.Spec.Filestore.StorageSize = sz
		changed = true
	}

	if instance.Spec.Ingress.Issuer == "" && r.Defaults.IngressIssuer != "" {
		instance.Spec.Ingress.Issuer = r.Defaults.IngressIssuer
		changed = true
	}
	if instance.Spec.Ingress.Class == nil && r.Defaults.IngressClass != "" {
		instance.Spec.Ingress.Class = &r.Defaults.IngressClass
		changed = true
	}

	if instance.Spec.Resources == nil && r.Defaults.Resources != nil {
		instance.Spec.Resources = r.Defaults.Resources.DeepCopy()
		changed = true
	}
	if instance.Spec.Affinity == nil && r.Defaults.Affinity != nil {
		instance.Spec.Affinity = r.Defaults.Affinity.DeepCopy()
		changed = true
	}
	if instance.Spec.Tolerations == nil && len(r.Defaults.Tolerations) > 0 {
		instance.Spec.Tolerations = append([]corev1.Toleration{}, r.Defaults.Tolerations...)
		changed = true
	}

	return changed
}

// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances/finalizers,verbs=update
// +kubebuilder:rbac:groups=bemade.org,resources=odooinitjobs,verbs=get;list;watch
// +kubebuilder:rbac:groups=bemade.org,resources=odooupgradejobs,verbs=get;list;watch
// +kubebuilder:rbac:groups=bemade.org,resources=odoorestorejobs,verbs=get;list;watch
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;create;update;patch
// +kubebuilder:rbac:groups=core,resources=services,verbs=get;list;watch;create;update;patch
// +kubebuilder:rbac:groups=core,resources=configmaps,verbs=get;list;watch;create;update;patch
// +kubebuilder:rbac:groups=core,resources=secrets,verbs=get;list;watch;create;update;patch
// +kubebuilder:rbac:groups=core,resources=persistentvolumeclaims,verbs=get;list;watch;create
// +kubebuilder:rbac:groups=networking.k8s.io,resources=ingresses,verbs=get;list;watch;create;update;patch

func (r *OdooInstanceReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var instance bemadev1alpha1.OdooInstance
	if err := r.Get(ctx, req.NamespacedName, &instance); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// Write operator-level defaults into any unset spec fields on the first
	// reconcile, then re-fetch so downstream logic works with the persisted
	// (correct ResourceVersion) copy.
	if r.applyDefaults(&instance) {
		if err := r.Update(ctx, &instance); err != nil {
			return ctrl.Result{}, fmt.Errorf("applying spec defaults: %w", err)
		}
		if err := r.Get(ctx, req.NamespacedName, &instance); err != nil {
			return ctrl.Result{}, client.IgnoreNotFound(err)
		}
	}

	// Load postgres cluster config (needed for db-config secret and Deployment env vars).
	// If spec.database.cluster was not set, loadPostgresCluster resolves the default
	// from the secret and returns its name so we can persist it into the spec.
	clusterName, pgCluster, err := r.loadPostgresCluster(ctx, instance.Spec.Database)
	if err != nil {
		log.Error(err, "failed to load postgres cluster config")
		return ctrl.Result{}, r.patchPhase(ctx, &instance, bemadev1alpha1.OdooInstancePhaseError,
			fmt.Sprintf("postgres cluster config: %v", err))
	}
	if instance.Spec.Database == nil || instance.Spec.Database.Cluster == "" {
		if instance.Spec.Database == nil {
			instance.Spec.Database = &bemadev1alpha1.DatabaseSpec{}
		}
		instance.Spec.Database.Cluster = clusterName
		if err := r.Update(ctx, &instance); err != nil {
			return ctrl.Result{}, fmt.Errorf("persisting default postgres cluster: %w", err)
		}
		if err := r.Get(ctx, req.NamespacedName, &instance); err != nil {
			return ctrl.Result{}, client.IgnoreNotFound(err)
		}
	}

	// Check whether a completed InitJob has appeared before ensuring child
	// resources, so desiredReplicas() sees the correct DBInitialized value
	// and the Deployment is created/updated with the right replica count.
	if !instance.Status.DBInitialized {
		if err := r.checkInitJobCompletion(ctx, &instance); err != nil {
			return ctrl.Result{}, err
		}
		// Re-read to pick up any status update just made.
		if err := r.Get(ctx, req.NamespacedName, &instance); err != nil {
			return ctrl.Result{}, client.IgnoreNotFound(err)
		}
	}

	// Ensure all child resources exist and reflect the current spec.
	if err := r.ensureChildResources(ctx, &instance, pgCluster); err != nil {
		return ctrl.Result{}, err
	}

	// Read current ready replica count from the Deployment.
	readyReplicas, err := r.deploymentReadyReplicas(ctx, &instance)
	if err != nil && !errors.IsNotFound(err) {
		return ctrl.Result{}, err
	}

	// Derive phase from observed state (priority-ordered).
	phase, err := r.derivePhase(ctx, &instance, readyReplicas)
	if err != nil {
		return ctrl.Result{}, err
	}

	// Build status URL from the first ingress host.
	url := ""
	if len(instance.Spec.Ingress.Hosts) > 0 {
		url = "https://" + instance.Spec.Ingress.Hosts[0]
	}

	patch := client.MergeFrom(instance.DeepCopy())
	instance.Status.Phase = phase
	instance.Status.ReadyReplicas = readyReplicas
	instance.Status.Ready = readyReplicas == instance.Spec.Replicas && instance.Spec.Replicas > 0
	instance.Status.URL = url
	if err := r.Status().Patch(ctx, &instance, patch); err != nil {
		return ctrl.Result{}, err
	}

	// Requeue while pods are starting or initialization is in progress.
	if phase == bemadev1alpha1.OdooInstancePhaseStarting ||
		phase == bemadev1alpha1.OdooInstancePhaseInitializing {
		return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
	}

	return ctrl.Result{}, nil
}

// ── Child resource management ─────────────────────────────────────────────────

func (r *OdooInstanceReconciler) ensureChildResources(ctx context.Context, instance *bemadev1alpha1.OdooInstance, pg postgresClusterConfig) error {
	if err := r.ensureOdooUserSecret(ctx, instance); err != nil {
		return fmt.Errorf("odoo-user secret: %w", err)
	}
	if err := r.ensureDBConfigSecret(ctx, instance, pg); err != nil {
		return fmt.Errorf("db-config secret: %w", err)
	}
	if err := r.ensureFilestorePVC(ctx, instance); err != nil {
		return fmt.Errorf("filestore pvc: %w", err)
	}
	if err := r.ensureConfigMap(ctx, instance); err != nil {
		return fmt.Errorf("odoo-conf configmap: %w", err)
	}
	if err := r.ensureService(ctx, instance); err != nil {
		return fmt.Errorf("service: %w", err)
	}
	if err := r.ensureIngress(ctx, instance); err != nil {
		return fmt.Errorf("ingress: %w", err)
	}
	if err := r.ensureDeployment(ctx, instance); err != nil {
		return fmt.Errorf("deployment: %w", err)
	}
	return nil
}

func (r *OdooInstanceReconciler) ensureOdooUserSecret(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name + "-odoo-user",
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, secret, func() error {
		if secret.UID == "" {
			// Generate credentials only on first creation.
			secret.Data = map[string][]byte{
				"username": []byte(odooUsername(instance.Namespace, instance.Name)),
				"password": []byte(generatePassword()),
			}
		}
		return controllerutil.SetControllerReference(instance, secret, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureDBConfigSecret(ctx context.Context, instance *bemadev1alpha1.OdooInstance, pg postgresClusterConfig) error {
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name + "-db-config",
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, secret, func() error {
		secret.Data = map[string][]byte{
			"host": []byte(pg.Host),
			"port": []byte(fmt.Sprintf("%d", pg.Port)),
		}
		return controllerutil.SetControllerReference(instance, secret, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureFilestorePVC(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	storageSize := instance.Spec.Filestore.StorageSize
	storageClass := instance.Spec.Filestore.StorageClass

	pvc := &corev1.PersistentVolumeClaim{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name + "-filestore-pvc",
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, pvc, func() error {
		if pvc.UID == "" {
			// Spec is immutable after creation — only set on first create.
			qty := resource.MustParse(storageSize)
			pvc.Spec = corev1.PersistentVolumeClaimSpec{
				AccessModes: []corev1.PersistentVolumeAccessMode{corev1.ReadWriteMany},
				Resources: corev1.VolumeResourceRequirements{
					Requests: corev1.ResourceList{corev1.ResourceStorage: qty},
				},
			}
			if storageClass != "" {
				pvc.Spec.StorageClassName = &storageClass
			}
		}
		return controllerutil.SetControllerReference(instance, pvc, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureConfigMap(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	username := odooUsername(instance.Namespace, instance.Name)
	cm := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name + "-odoo-conf",
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, cm, func() error {
		cm.Data = map[string]string{
			"odoo.conf": buildOdooConf(username, instance.Spec.AdminPassword, instance.Spec.ConfigOptions),
		}
		return controllerutil.SetControllerReference(instance, cm, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureService(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	svc := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name,
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, svc, func() error {
		svc.Labels = map[string]string{"app": instance.Name}
		svc.Spec.Selector = map[string]string{"app": instance.Name}
		svc.Spec.Type = corev1.ServiceTypeClusterIP
		svc.Spec.Ports = []corev1.ServicePort{
			{Name: "http", Port: 8069, TargetPort: intstr.FromInt(8069), Protocol: corev1.ProtocolTCP},
			{Name: "websocket", Port: 8072, TargetPort: intstr.FromInt(8072), Protocol: corev1.ProtocolTCP},
		}
		return controllerutil.SetControllerReference(instance, svc, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureIngress(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	pathTypePrefix := networkingv1.PathTypePrefix
	ing := &networkingv1.Ingress{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name,
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, ing, func() error {
		if ing.Annotations == nil {
			ing.Annotations = map[string]string{}
		}
		if instance.Spec.Ingress.Issuer != "" {
			ing.Annotations["cert-manager.io/cluster-issuer"] = instance.Spec.Ingress.Issuer
		}
		if instance.Spec.Ingress.Class != nil {
			ing.Spec.IngressClassName = instance.Spec.Ingress.Class
		}

		var rules []networkingv1.IngressRule
		for _, host := range instance.Spec.Ingress.Hosts {
			rules = append(rules, networkingv1.IngressRule{
				Host: host,
				IngressRuleValue: networkingv1.IngressRuleValue{
					HTTP: &networkingv1.HTTPIngressRuleValue{
						Paths: []networkingv1.HTTPIngressPath{
							{
								Path:     "/websocket",
								PathType: &pathTypePrefix,
								Backend: networkingv1.IngressBackend{
									Service: &networkingv1.IngressServiceBackend{
										Name: instance.Name,
										Port: networkingv1.ServiceBackendPort{Number: 8072},
									},
								},
							},
							{
								Path:     "/",
								PathType: &pathTypePrefix,
								Backend: networkingv1.IngressBackend{
									Service: &networkingv1.IngressServiceBackend{
										Name: instance.Name,
										Port: networkingv1.ServiceBackendPort{Number: 8069},
									},
								},
							},
						},
					},
				},
			})
		}
		ing.Spec.Rules = rules
		ing.Spec.TLS = []networkingv1.IngressTLS{
			{
				Hosts:      instance.Spec.Ingress.Hosts,
				SecretName: instance.Name + "-tls",
			},
		}
		return controllerutil.SetControllerReference(instance, ing, r.Scheme)
	})
	return err
}

func (r *OdooInstanceReconciler) ensureDeployment(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	replicas := desiredReplicas(instance)
	dep := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      instance.Name,
			Namespace: instance.Namespace,
		},
	}
	_, err := controllerutil.CreateOrPatch(ctx, r.Client, dep, func() error {
		dep.Labels = map[string]string{"app": instance.Name}
		dep.Spec.Replicas = &replicas
		dep.Spec.Selector = &metav1.LabelSelector{
			MatchLabels: map[string]string{"app": instance.Name},
		}

		strategy := appsv1.RecreateDeploymentStrategyType
		if instance.Spec.Strategy != nil && instance.Spec.Strategy.Type == bemadev1alpha1.DeploymentStrategyRollingUpdate {
			strategy = appsv1.RollingUpdateDeploymentStrategyType
		}
		dep.Spec.Strategy = appsv1.DeploymentStrategy{Type: strategy}

		image := instance.Spec.Image

		var imagePullSecrets []corev1.LocalObjectReference
		if instance.Spec.ImagePullSecret != "" {
			imagePullSecrets = []corev1.LocalObjectReference{{Name: instance.Spec.ImagePullSecret}}
		}

		probeStartup := "/web/health"
		probeLiveness := "/web/health"
		probeReadiness := "/web/health"
		if instance.Spec.Probes != nil {
			if instance.Spec.Probes.StartupPath != "" {
				probeStartup = instance.Spec.Probes.StartupPath
			}
			if instance.Spec.Probes.LivenessPath != "" {
				probeLiveness = instance.Spec.Probes.LivenessPath
			}
			if instance.Spec.Probes.ReadinessPath != "" {
				probeReadiness = instance.Spec.Probes.ReadinessPath
			}
		}

		dbSecretName := instance.Name + "-odoo-user"
		dbConfigSecretName := instance.Name + "-db-config"

		dep.Spec.Template = corev1.PodTemplateSpec{
			ObjectMeta: metav1.ObjectMeta{Labels: map[string]string{"app": instance.Name}},
			Spec: corev1.PodSpec{
				ImagePullSecrets: imagePullSecrets,
				Affinity:         instance.Spec.Affinity,
				Tolerations:      instance.Spec.Tolerations,
				SecurityContext: &corev1.PodSecurityContext{
					RunAsUser:           ptr(int64(100)),
					RunAsGroup:          ptr(int64(101)),
					FSGroup:             ptr(int64(101)),
					FSGroupChangePolicy: ptr(corev1.FSGroupChangeOnRootMismatch),
				},
				Volumes: []corev1.Volume{
					{
						Name: "filestore",
						VolumeSource: corev1.VolumeSource{
							PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{
								ClaimName: instance.Name + "-filestore-pvc",
							},
						},
					},
					{
						Name: "odoo-conf",
						VolumeSource: corev1.VolumeSource{
							ConfigMap: &corev1.ConfigMapVolumeSource{
								LocalObjectReference: corev1.LocalObjectReference{Name: instance.Name + "-odoo-conf"},
							},
						},
					},
				},
				Containers: []corev1.Container{
					{
						Name:            "odoo-" + instance.Name,
						Image:           image,
						ImagePullPolicy: corev1.PullIfNotPresent,
						Command:         []string{"/entrypoint.sh", "odoo"},
						Ports: []corev1.ContainerPort{
							{Name: "http", ContainerPort: 8069},
							{Name: "websocket", ContainerPort: 8072},
						},
						VolumeMounts: []corev1.VolumeMount{
							{Name: "filestore", MountPath: "/var/lib/odoo"},
							{Name: "odoo-conf", MountPath: "/etc/odoo"},
						},
						Env: []corev1.EnvVar{
							{
								Name: "HOST",
								ValueFrom: &corev1.EnvVarSource{
									SecretKeyRef: &corev1.SecretKeySelector{
										LocalObjectReference: corev1.LocalObjectReference{Name: dbConfigSecretName},
										Key:                  "host",
									},
								},
							},
							{
								Name: "PORT",
								ValueFrom: &corev1.EnvVarSource{
									SecretKeyRef: &corev1.SecretKeySelector{
										LocalObjectReference: corev1.LocalObjectReference{Name: dbConfigSecretName},
										Key:                  "port",
									},
								},
							},
							{
								Name: "USER",
								ValueFrom: &corev1.EnvVarSource{
									SecretKeyRef: &corev1.SecretKeySelector{
										LocalObjectReference: corev1.LocalObjectReference{Name: dbSecretName},
										Key:                  "username",
									},
								},
							},
							{
								Name: "PASSWORD",
								ValueFrom: &corev1.EnvVarSource{
									SecretKeyRef: &corev1.SecretKeySelector{
										LocalObjectReference: corev1.LocalObjectReference{Name: dbSecretName},
										Key:                  "password",
									},
								},
							},
						},
						Resources: func() corev1.ResourceRequirements {
							if instance.Spec.Resources != nil {
								return *instance.Spec.Resources
							}
							return corev1.ResourceRequirements{}
						}(),
						StartupProbe: &corev1.Probe{
							ProbeHandler: corev1.ProbeHandler{
								HTTPGet: &corev1.HTTPGetAction{
									Path: probeStartup,
									Port: intstr.FromInt(8069),
								},
							},
							InitialDelaySeconds: 5,
							PeriodSeconds:       10,
							TimeoutSeconds:      5,
							FailureThreshold:    30,
						},
						LivenessProbe: &corev1.Probe{
							ProbeHandler: corev1.ProbeHandler{
								HTTPGet: &corev1.HTTPGetAction{
									Path: probeLiveness,
									Port: intstr.FromInt(8069),
								},
							},
							PeriodSeconds:    15,
							TimeoutSeconds:   5,
							FailureThreshold: 3,
						},
						ReadinessProbe: &corev1.Probe{
							ProbeHandler: corev1.ProbeHandler{
								HTTPGet: &corev1.HTTPGetAction{
									Path: probeReadiness,
									Port: intstr.FromInt(8069),
								},
							},
							PeriodSeconds:    10,
							TimeoutSeconds:   5,
							FailureThreshold: 3,
						},
					},
				},
			},
		}
		return controllerutil.SetControllerReference(instance, dep, r.Scheme)
	})
	return err
}

// ── Phase derivation ──────────────────────────────────────────────────────────

func (r *OdooInstanceReconciler) derivePhase(ctx context.Context, instance *bemadev1alpha1.OdooInstance, readyReplicas int32) (bemadev1alpha1.OdooInstancePhase, error) {
	// 1. Stopped — explicit user intent.
	if instance.Spec.Replicas == 0 {
		return bemadev1alpha1.OdooInstancePhaseStopped, nil
	}

	// 2–5. Job-driven phases (restore takes priority over upgrade).
	activeRestore, failedRestore, err := r.getRestoreJobState(ctx, instance)
	if err != nil {
		return "", err
	}
	activeUpgrade, failedUpgrade, err := r.getUpgradeJobState(ctx, instance)
	if err != nil {
		return "", err
	}
	if activeRestore {
		return bemadev1alpha1.OdooInstancePhaseRestoring, nil
	}
	if activeUpgrade {
		return bemadev1alpha1.OdooInstancePhaseUpgrading, nil
	}
	if failedRestore {
		return bemadev1alpha1.OdooInstancePhaseRestoreFailed, nil
	}
	if failedUpgrade {
		return bemadev1alpha1.OdooInstancePhaseUpgradeFailed, nil
	}

	// 6–8. Init-driven phases.
	if !instance.Status.DBInitialized {
		initPhase, err := r.latestInitJobPhase(ctx, instance)
		if err != nil {
			return "", err
		}
		switch initPhase {
		case bemadev1alpha1.PhaseRunning:
			return bemadev1alpha1.OdooInstancePhaseInitializing, nil
		case bemadev1alpha1.PhaseFailed:
			return bemadev1alpha1.OdooInstancePhaseInitFailed, nil
		default:
			return bemadev1alpha1.OdooInstancePhaseUninitialized, nil
		}
	}

	// 9–11. Deployment-driven phases.
	if readyReplicas == 0 {
		return bemadev1alpha1.OdooInstancePhaseStarting, nil
	}
	if readyReplicas < instance.Spec.Replicas {
		return bemadev1alpha1.OdooInstancePhaseDegraded, nil
	}
	return bemadev1alpha1.OdooInstancePhaseRunning, nil
}

func (r *OdooInstanceReconciler) checkInitJobCompletion(ctx context.Context, instance *bemadev1alpha1.OdooInstance) error {
	var list bemadev1alpha1.OdooInitJobList
	if err := r.List(ctx, &list, client.InNamespace(instance.Namespace)); err != nil {
		return err
	}
	for _, job := range list.Items {
		if job.Spec.OdooInstanceRef.Name == instance.Name &&
			job.Status.Phase == bemadev1alpha1.PhaseCompleted {
			patch := client.MergeFrom(instance.DeepCopy())
			instance.Status.DBInitialized = true
			return r.Status().Patch(ctx, instance, patch)
		}
	}
	return nil
}

func (r *OdooInstanceReconciler) getRestoreJobState(ctx context.Context, instance *bemadev1alpha1.OdooInstance) (active, failed bool, err error) {
	var list bemadev1alpha1.OdooRestoreJobList
	if err = r.List(ctx, &list, client.InNamespace(instance.Namespace)); err != nil {
		return
	}
	for _, job := range list.Items {
		if job.Spec.OdooInstanceRef.Name != instance.Name {
			continue
		}
		if job.Status.Phase == bemadev1alpha1.PhaseRunning {
			active = true
		}
		if job.Status.Phase == bemadev1alpha1.PhaseFailed {
			failed = true
		}
	}
	return
}

func (r *OdooInstanceReconciler) getUpgradeJobState(ctx context.Context, instance *bemadev1alpha1.OdooInstance) (active, failed bool, err error) {
	var list bemadev1alpha1.OdooUpgradeJobList
	if err = r.List(ctx, &list, client.InNamespace(instance.Namespace)); err != nil {
		return
	}
	for _, job := range list.Items {
		if job.Spec.OdooInstanceRef.Name != instance.Name {
			continue
		}
		if job.Status.Phase == bemadev1alpha1.PhaseRunning {
			active = true
		}
		if job.Status.Phase == bemadev1alpha1.PhaseFailed {
			failed = true
		}
	}
	return
}

func (r *OdooInstanceReconciler) latestInitJobPhase(ctx context.Context, instance *bemadev1alpha1.OdooInstance) (bemadev1alpha1.Phase, error) {
	var list bemadev1alpha1.OdooInitJobList
	if err := r.List(ctx, &list, client.InNamespace(instance.Namespace)); err != nil {
		return "", err
	}
	var result bemadev1alpha1.Phase
	for _, job := range list.Items {
		if job.Spec.OdooInstanceRef.Name != instance.Name {
			continue
		}
		switch job.Status.Phase {
		case bemadev1alpha1.PhaseRunning:
			return bemadev1alpha1.PhaseRunning, nil
		case bemadev1alpha1.PhaseFailed:
			result = bemadev1alpha1.PhaseFailed
		}
	}
	return result, nil
}

// ── Postgres cluster config ───────────────────────────────────────────────────

// loadPostgresCluster resolves the postgres cluster for the given instance and
// returns both the cluster name and its configuration. If spec.database.cluster
// is set it is used directly; otherwise the cluster marked default:true in the
// secret is used. The returned name can be written back to spec.database.cluster
// so the spec becomes self-describing.
func (r *OdooInstanceReconciler) loadPostgresCluster(ctx context.Context, dbSpec *bemadev1alpha1.DatabaseSpec) (string, postgresClusterConfig, error) {
	secretName := r.PostgresClustersSecret
	if secretName == "" {
		secretName = "postgres-clusters"
	}
	var secret corev1.Secret
	if err := r.Get(ctx, types.NamespacedName{Name: secretName, Namespace: r.OperatorNamespace}, &secret); err != nil {
		return "", postgresClusterConfig{}, fmt.Errorf("%s secret: %w", secretName, err)
	}

	raw, ok := secret.Data["clusters.yaml"]
	if !ok {
		return "", postgresClusterConfig{}, fmt.Errorf("postgres-clusters secret missing clusters.yaml key")
	}

	var clusters map[string]postgresClusterConfig
	if err := sigsyaml.Unmarshal(raw, &clusters); err != nil {
		return "", postgresClusterConfig{}, fmt.Errorf("parsing clusters.yaml: %w", err)
	}

	if dbSpec != nil && dbSpec.Cluster != "" {
		c, ok := clusters[dbSpec.Cluster]
		if !ok {
			return "", postgresClusterConfig{}, fmt.Errorf("postgres cluster %q not found", dbSpec.Cluster)
		}
		return dbSpec.Cluster, c, nil
	}

	for name, c := range clusters {
		if c.Default {
			return name, c, nil
		}
	}
	return "", postgresClusterConfig{}, fmt.Errorf("no default postgres cluster configured in %s secret", secretName)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

func (r *OdooInstanceReconciler) deploymentReadyReplicas(ctx context.Context, instance *bemadev1alpha1.OdooInstance) (int32, error) {
	var dep appsv1.Deployment
	if err := r.Get(ctx, types.NamespacedName{Name: instance.Name, Namespace: instance.Namespace}, &dep); err != nil {
		return 0, err
	}
	return dep.Status.ReadyReplicas, nil
}

func (r *OdooInstanceReconciler) patchPhase(ctx context.Context, instance *bemadev1alpha1.OdooInstance, phase bemadev1alpha1.OdooInstancePhase, message string) error {
	patch := client.MergeFrom(instance.DeepCopy())
	instance.Status.Phase = phase
	instance.Status.Message = message
	return r.Status().Patch(ctx, instance, patch)
}

func desiredReplicas(instance *bemadev1alpha1.OdooInstance) int32 {
	if !instance.Status.DBInitialized {
		return 0
	}
	return instance.Spec.Replicas
}

func odooUsername(namespace, name string) string {
	return fmt.Sprintf("odoo.%s.%s", namespace, name)
}

func generatePassword() string {
	b := make([]byte, 24)
	if _, err := rand.Read(b); err != nil {
		panic(fmt.Sprintf("failed to generate random password: %v", err))
	}
	return hex.EncodeToString(b)
}

// buildOdooConf generates the content of odoo.conf.
// Note: admin_passwd is stored in plaintext. Odoo accepts plaintext and will
// hash it on first write, but since the ConfigMap is read-only from the pod's
// perspective it stays as-is. The value is already present in OdooInstance.spec
// so this does not increase the attack surface.
func buildOdooConf(username, adminPassword string, extra map[string]string) string {
	options := map[string]string{
		"data_dir":       "/var/lib/odoo",
		"logfile":        "",
		"log_level":      "info",
		"proxy_mode":     "True",
		"addons_path":    "/mnt/extra-addons",
		"db_user":        username,
		"list_db":        "False",
		"http_interface": "0.0.0.0",
		"http_port":      "8069",
	}
	if adminPassword != "" {
		options["admin_passwd"] = adminPassword
	}
	for k, v := range extra {
		options[k] = v
	}

	var sb strings.Builder
	sb.WriteString("[options]\n")
	// Write standard keys in a stable order.
	keys := []string{
		"data_dir", "logfile", "log_level", "proxy_mode", "addons_path",
		"db_user", "list_db", "http_interface", "http_port", "admin_passwd",
	}
	written := map[string]bool{}
	for _, k := range keys {
		if v, ok := options[k]; ok {
			sb.WriteString(fmt.Sprintf("%s = %s\n", k, v))
			written[k] = true
		}
	}
	for k, v := range options {
		if !written[k] {
			sb.WriteString(fmt.Sprintf("%s = %s\n", k, v))
		}
	}
	return sb.String()
}

// ── Controller wiring ─────────────────────────────────────────────────────────

func (r *OdooInstanceReconciler) SetupWithManager(mgr ctrl.Manager) error {
	jobToInstance := func(_ context.Context, obj client.Object) []reconcile.Request {
		var instanceName string
		switch v := obj.(type) {
		case *bemadev1alpha1.OdooInitJob:
			instanceName = v.Spec.OdooInstanceRef.Name
		case *bemadev1alpha1.OdooUpgradeJob:
			instanceName = v.Spec.OdooInstanceRef.Name
		case *bemadev1alpha1.OdooRestoreJob:
			instanceName = v.Spec.OdooInstanceRef.Name
		}
		if instanceName == "" {
			return nil
		}
		return []reconcile.Request{{
			NamespacedName: types.NamespacedName{Name: instanceName, Namespace: obj.GetNamespace()},
		}}
	}

	return ctrl.NewControllerManagedBy(mgr).
		For(&bemadev1alpha1.OdooInstance{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Owns(&networkingv1.Ingress{}).
		Owns(&corev1.ConfigMap{}).
		Owns(&corev1.Secret{}).
		Owns(&corev1.PersistentVolumeClaim{}).
		Watches(&bemadev1alpha1.OdooInitJob{}, handler.EnqueueRequestsFromMapFunc(jobToInstance)).
		Watches(&bemadev1alpha1.OdooUpgradeJob{}, handler.EnqueueRequestsFromMapFunc(jobToInstance)).
		Watches(&bemadev1alpha1.OdooRestoreJob{}, handler.EnqueueRequestsFromMapFunc(jobToInstance)).
		Named("odooinstance").
		Complete(r)
}
