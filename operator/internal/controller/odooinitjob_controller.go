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
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"

	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	logf "sigs.k8s.io/controller-runtime/pkg/log"

	bemadev1alpha1 "github.com/bemade/odoo-operator/operator/api/v1alpha1"
)

// OdooInitJobReconciler reconciles a OdooInitJob object
type OdooInitJobReconciler struct {
	client.Client
	Scheme     *runtime.Scheme
	HTTPClient *http.Client
}

// +kubebuilder:rbac:groups=bemade.org,resources=odooinitjobs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=bemade.org,resources=odooinitjobs/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=bemade.org,resources=odooinitjobs/finalizers,verbs=update
// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances,verbs=get;list;watch
// +kubebuilder:rbac:groups=batch,resources=jobs,verbs=get;list;watch;create;delete
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;patch
// +kubebuilder:rbac:groups=core,resources=secrets,verbs=get

func (r *OdooInitJobReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	// Fetch the OdooInitJob
	var initJob bemadev1alpha1.OdooInitJob
	if err := r.Get(ctx, req.NamespacedName, &initJob); err != nil {
		// Not found means it was deleted — nothing to do
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// Terminal states — nothing further to reconcile
	if initJob.Status.Phase == bemadev1alpha1.PhaseCompleted ||
		initJob.Status.Phase == bemadev1alpha1.PhaseFailed {
		return ctrl.Result{}, nil
	}

	// No job created yet — this is a fresh OdooInitJob
	if initJob.Status.JobName == "" {
		return r.startJob(ctx, &initJob)
	}

	// Job was created — check its current state
	log.Info("checking job status", "job", initJob.Status.JobName)
	return r.syncJobStatus(ctx, &initJob)
}

// startJob looks up the referenced OdooInstance, scales it down, creates the
// init Job, and sets status to Running.
func (r *OdooInitJobReconciler) startJob(ctx context.Context, initJob *bemadev1alpha1.OdooInitJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	instanceNS := initJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = initJob.Namespace
	}
	instanceName := initJob.Spec.OdooInstanceRef.Name

	var odooInstance bemadev1alpha1.OdooInstance
	if err := r.Get(ctx, types.NamespacedName{Name: instanceName, Namespace: instanceNS}, &odooInstance); err != nil {
		if errors.IsNotFound(err) {
			return r.setFailed(ctx, initJob, fmt.Sprintf("OdooInstance %s not found", instanceName))
		}
		return ctrl.Result{}, err
	}

	// Scale the deployment to 0 before initialising
	if err := scaleDeployment(ctx, r.Client, instanceName, instanceNS, 0); err != nil {
		log.Error(err, "failed to scale down deployment — proceeding anyway", "instance", instanceName)
	}

	job, err := r.buildInitJob(initJob, &odooInstance)
	if err != nil {
		return r.setFailed(ctx, initJob, fmt.Sprintf("failed to build job: %v", err))
	}

	if err := r.Create(ctx, job); err != nil {
		return ctrl.Result{}, fmt.Errorf("creating init job: %w", err)
	}

	log.Info("created init job", "job", job.Name)

	patch := client.MergeFrom(initJob.DeepCopy())
	initJob.Status.Phase = bemadev1alpha1.PhaseRunning
	initJob.Status.JobName = job.Name
	now := metav1.Now()
	initJob.Status.StartTime = &now
	if err := r.Status().Patch(ctx, initJob, patch); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating status to Running: %w", err)
	}

	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

// syncJobStatus fetches the underlying Job and updates OdooInitJob status accordingly.
func (r *OdooInitJobReconciler) syncJobStatus(ctx context.Context, initJob *bemadev1alpha1.OdooInitJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var job batchv1.Job
	if err := r.Get(ctx, types.NamespacedName{Name: initJob.Status.JobName, Namespace: initJob.Namespace}, &job); err != nil {
		if errors.IsNotFound(err) {
			log.Info("job not found, may have been deleted", "job", initJob.Status.JobName)
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	if job.Status.Succeeded > 0 {
		log.Info("init job succeeded")
		return ctrl.Result{}, r.finalise(ctx, initJob, bemadev1alpha1.PhaseCompleted, "")
	}

	if job.Status.Failed > 0 {
		log.Info("init job failed")
		return ctrl.Result{}, r.finalise(ctx, initJob, bemadev1alpha1.PhaseFailed, "init job failed")
	}

	// Still running — the Owns() watch will trigger reconciliation when it
	// completes, but requeue as a safety net
	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

// finalise sets a terminal status, scales the instance back up, and fires the webhook.
func (r *OdooInitJobReconciler) finalise(ctx context.Context, initJob *bemadev1alpha1.OdooInitJob, phase bemadev1alpha1.Phase, message string) error {
	patch := client.MergeFrom(initJob.DeepCopy())
	initJob.Status.Phase = phase
	initJob.Status.Message = message
	now := metav1.Now()
	initJob.Status.CompletionTime = &now
	if err := r.Status().Patch(ctx, initJob, patch); err != nil {
		return fmt.Errorf("updating terminal status: %w", err)
	}

	instanceNS := initJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = initJob.Namespace
	}
	if err := r.scaleInstanceBackUp(ctx, initJob.Spec.OdooInstanceRef.Name, instanceNS); err != nil {
		logf.FromContext(ctx).Error(err, "failed to scale instance back up")
	}

	if initJob.Spec.Webhook != nil {
		r.notifyWebhook(ctx, initJob, phase)
	}

	return nil
}

// setFailed is a convenience wrapper for immediate failure before a job is created.
func (r *OdooInitJobReconciler) setFailed(ctx context.Context, initJob *bemadev1alpha1.OdooInitJob, message string) (ctrl.Result, error) {
	patch := client.MergeFrom(initJob.DeepCopy())
	initJob.Status.Phase = bemadev1alpha1.PhaseFailed
	initJob.Status.Message = message
	return ctrl.Result{}, r.Status().Patch(ctx, initJob, patch)
}

// buildInitJob constructs the batch/v1 Job that runs `odoo -i <modules>`.
func (r *OdooInitJobReconciler) buildInitJob(initJob *bemadev1alpha1.OdooInitJob, odooInstance *bemadev1alpha1.OdooInstance) (*batchv1.Job, error) {
	instanceName := odooInstance.Name
	instanceUID := string(odooInstance.UID)

	image := odooInstance.Spec.Image
	if image == "" {
		image = "odoo:18.0"
	}

	var imagePullSecrets []corev1.LocalObjectReference
	if odooInstance.Spec.ImagePullSecret != "" {
		imagePullSecrets = []corev1.LocalObjectReference{{Name: odooInstance.Spec.ImagePullSecret}}
	}

	dbSecretName := fmt.Sprintf("%s-odoo-user", instanceName)
	dbName := fmt.Sprintf("odoo_%s", sanitiseUID(instanceUID))

	modules := initJob.Spec.Modules
	if len(modules) == 0 {
		modules = []string{"base"}
	}

	ttl := int32(900)
	backoffLimit := int32(0)

	job := &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			GenerateName: fmt.Sprintf("%s-", initJob.Name),
			Namespace:    initJob.Namespace,
		},
		Spec: batchv1.JobSpec{
			BackoffLimit:            &backoffLimit,
			TTLSecondsAfterFinished: &ttl,
			Template: corev1.PodTemplateSpec{
				Spec: corev1.PodSpec{
					RestartPolicy:    corev1.RestartPolicyNever,
					ImagePullSecrets: imagePullSecrets,
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
									ClaimName: fmt.Sprintf("%s-filestore-pvc", instanceName),
								},
							},
						},
						{
							Name: "odoo-conf",
							VolumeSource: corev1.VolumeSource{
								ConfigMap: &corev1.ConfigMapVolumeSource{
									LocalObjectReference: corev1.LocalObjectReference{
										Name: fmt.Sprintf("%s-odoo-conf", instanceName),
									},
								},
							},
						},
					},
					Containers: []corev1.Container{
						{
							Name:    "init",
							Image:   image,
							Command: []string{"/entrypoint.sh", "odoo"},
							Args:    []string{"-i", strings.Join(modules, ","), "-d", dbName, "--no-http", "--stop-after-init"},
							Env: []corev1.EnvVar{
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
							VolumeMounts: []corev1.VolumeMount{
								{Name: "filestore", MountPath: "/var/lib/odoo"},
								{Name: "odoo-conf", MountPath: "/etc/odoo"},
							},
						},
					},
				},
			},
		},
	}

	if err := controllerutil.SetControllerReference(initJob, job, r.Scheme); err != nil {
		return nil, err
	}
	return job, nil
}

// scaleInstanceBackUp reads desired replicas from the OdooInstance and scales the Deployment back up.
func (r *OdooInitJobReconciler) scaleInstanceBackUp(ctx context.Context, instanceName, instanceNS string) error {
	var odooInstance bemadev1alpha1.OdooInstance
	if err := r.Get(ctx, types.NamespacedName{Name: instanceName, Namespace: instanceNS}, &odooInstance); err != nil {
		return err
	}
	replicas := odooInstance.Spec.Replicas
	if replicas == 0 {
		replicas = 1
	}
	return scaleDeployment(ctx, r.Client, instanceName, instanceNS, replicas)
}

// notifyWebhook POSTs a status payload to the configured webhook URL.
func (r *OdooInitJobReconciler) notifyWebhook(ctx context.Context, initJob *bemadev1alpha1.OdooInitJob, phase bemadev1alpha1.Phase) {
	log := logf.FromContext(ctx)
	wh := initJob.Spec.Webhook

	token := wh.Token
	if token == "" && wh.SecretTokenSecretRef != nil {
		var secret corev1.Secret
		if err := r.Get(ctx, types.NamespacedName{
			Name:      wh.SecretTokenSecretRef.Name,
			Namespace: initJob.Namespace,
		}, &secret); err == nil {
			token = string(secret.Data[wh.SecretTokenSecretRef.Key])
		}
	}

	payload, _ := json.Marshal(map[string]any{
		"initJob":        initJob.Name,
		"namespace":      initJob.Namespace,
		"phase":          phase,
		"targetInstance": initJob.Spec.OdooInstanceRef.Name,
		"modules":        initJob.Spec.Modules,
	})

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, wh.URL, bytes.NewReader(payload))
	if err != nil {
		log.Error(err, "failed to build webhook request")
		return
	}
	req.Header.Set("Content-Type", "application/json")
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}

	httpClient := r.HTTPClient
	if httpClient == nil {
		httpClient = http.DefaultClient
	}
	resp, err := httpClient.Do(req)
	if err != nil {
		log.Error(err, "webhook notification failed")
		return
	}
	defer resp.Body.Close()
	log.Info("webhook notification sent", "status", resp.StatusCode)
}

// SetupWithManager sets up the controller with the Manager.
func (r *OdooInitJobReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&bemadev1alpha1.OdooInitJob{}).
		// Trigger reconciliation whenever an owned Job changes (e.g. completes)
		Owns(&batchv1.Job{}).
		Named("odooinitjob").
		Complete(r)
}

// ptr, sanitiseUID and scaleDeployment are defined in helpers.go
