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

// OdooUpgradeJobReconciler reconciles a OdooUpgradeJob object.
type OdooUpgradeJobReconciler struct {
	client.Client
	Scheme     *runtime.Scheme
	HTTPClient *http.Client
}

// +kubebuilder:rbac:groups=bemade.org,resources=odooupgradejobs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=bemade.org,resources=odooupgradejobs/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=bemade.org,resources=odooupgradejobs/finalizers,verbs=update
// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances,verbs=get;list;watch
// +kubebuilder:rbac:groups=batch,resources=jobs,verbs=get;list;watch;create;delete
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;patch
// +kubebuilder:rbac:groups=core,resources=secrets,verbs=get

func (r *OdooUpgradeJobReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var upgradeJob bemadev1alpha1.OdooUpgradeJob
	if err := r.Get(ctx, req.NamespacedName, &upgradeJob); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	if upgradeJob.Status.Phase == bemadev1alpha1.PhaseCompleted ||
		upgradeJob.Status.Phase == bemadev1alpha1.PhaseFailed {
		return ctrl.Result{}, nil
	}

	// Respect scheduledTime — wait until the scheduled moment arrives.
	if upgradeJob.Spec.ScheduledTime != nil && time.Now().Before(upgradeJob.Spec.ScheduledTime.Time) {
		waitFor := time.Until(upgradeJob.Spec.ScheduledTime.Time)
		log.Info("upgrade scheduled in the future, requeueing", "in", waitFor)
		return ctrl.Result{RequeueAfter: waitFor}, nil
	}

	if upgradeJob.Status.JobName == "" {
		return r.startJob(ctx, &upgradeJob)
	}

	log.Info("checking upgrade job status", "job", upgradeJob.Status.JobName)
	return r.syncJobStatus(ctx, &upgradeJob)
}

func (r *OdooUpgradeJobReconciler) startJob(ctx context.Context, upgradeJob *bemadev1alpha1.OdooUpgradeJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	instanceNS := upgradeJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = upgradeJob.Namespace
	}
	instanceName := upgradeJob.Spec.OdooInstanceRef.Name

	var odooInstance bemadev1alpha1.OdooInstance
	if err := r.Get(ctx, types.NamespacedName{Name: instanceName, Namespace: instanceNS}, &odooInstance); err != nil {
		if errors.IsNotFound(err) {
			return r.setFailed(ctx, upgradeJob, fmt.Sprintf("OdooInstance %s not found", instanceName))
		}
		return ctrl.Result{}, err
	}

	// Record original replicas so we can restore after completion.
	originalReplicas := odooInstance.Spec.Replicas
	if originalReplicas == 0 {
		originalReplicas = 1
	}

	if err := scaleDeployment(ctx, r.Client, instanceName, instanceNS, 0); err != nil {
		log.Error(err, "failed to scale down deployment — proceeding anyway", "instance", instanceName)
	}

	job, err := r.buildUpgradeJob(upgradeJob, &odooInstance)
	if err != nil {
		return r.setFailed(ctx, upgradeJob, fmt.Sprintf("failed to build job: %v", err))
	}

	if err := r.Create(ctx, job); err != nil {
		return ctrl.Result{}, fmt.Errorf("creating upgrade job: %w", err)
	}

	log.Info("created upgrade job", "job", job.Name)

	patch := client.MergeFrom(upgradeJob.DeepCopy())
	upgradeJob.Status.Phase = bemadev1alpha1.PhaseRunning
	upgradeJob.Status.JobName = job.Name
	upgradeJob.Status.OriginalReplicas = originalReplicas
	now := metav1.Now()
	upgradeJob.Status.StartTime = &now
	if err := r.Status().Patch(ctx, upgradeJob, patch); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating status to Running: %w", err)
	}

	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

func (r *OdooUpgradeJobReconciler) syncJobStatus(ctx context.Context, upgradeJob *bemadev1alpha1.OdooUpgradeJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var job batchv1.Job
	if err := r.Get(ctx, types.NamespacedName{Name: upgradeJob.Status.JobName, Namespace: upgradeJob.Namespace}, &job); err != nil {
		if errors.IsNotFound(err) {
			log.Info("job not found, may have been deleted", "job", upgradeJob.Status.JobName)
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	if job.Status.Succeeded > 0 {
		log.Info("upgrade job succeeded")
		return ctrl.Result{}, r.finalise(ctx, upgradeJob, bemadev1alpha1.PhaseCompleted, "")
	}
	if job.Status.Failed > 0 {
		log.Info("upgrade job failed")
		return ctrl.Result{}, r.finalise(ctx, upgradeJob, bemadev1alpha1.PhaseFailed, "upgrade job failed")
	}

	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

func (r *OdooUpgradeJobReconciler) finalise(ctx context.Context, upgradeJob *bemadev1alpha1.OdooUpgradeJob, phase bemadev1alpha1.Phase, message string) error {
	patch := client.MergeFrom(upgradeJob.DeepCopy())
	upgradeJob.Status.Phase = phase
	upgradeJob.Status.Message = message
	now := metav1.Now()
	upgradeJob.Status.CompletionTime = &now
	if err := r.Status().Patch(ctx, upgradeJob, patch); err != nil {
		return fmt.Errorf("updating terminal status: %w", err)
	}

	instanceNS := upgradeJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = upgradeJob.Namespace
	}
	replicas := upgradeJob.Status.OriginalReplicas
	if replicas == 0 {
		replicas = 1
	}
	if err := scaleDeployment(ctx, r.Client, upgradeJob.Spec.OdooInstanceRef.Name, instanceNS, replicas); err != nil {
		logf.FromContext(ctx).Error(err, "failed to scale instance back up")
	}

	if upgradeJob.Spec.Webhook != nil {
		r.notifyWebhook(ctx, upgradeJob, phase)
	}

	return nil
}

func (r *OdooUpgradeJobReconciler) setFailed(ctx context.Context, upgradeJob *bemadev1alpha1.OdooUpgradeJob, message string) (ctrl.Result, error) {
	patch := client.MergeFrom(upgradeJob.DeepCopy())
	upgradeJob.Status.Phase = bemadev1alpha1.PhaseFailed
	upgradeJob.Status.Message = message
	return ctrl.Result{}, r.Status().Patch(ctx, upgradeJob, patch)
}

func (r *OdooUpgradeJobReconciler) buildUpgradeJob(upgradeJob *bemadev1alpha1.OdooUpgradeJob, odooInstance *bemadev1alpha1.OdooInstance) (*batchv1.Job, error) {
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

	// Build -u and -i argument lists.
	var args []string
	args = append(args, "-d", dbName, "--no-http", "--stop-after-init")
	if len(upgradeJob.Spec.Modules) > 0 {
		args = append(args, "-u", strings.Join(upgradeJob.Spec.Modules, ","))
	}
	if len(upgradeJob.Spec.ModulesInstall) > 0 {
		args = append(args, "-i", strings.Join(upgradeJob.Spec.ModulesInstall, ","))
	}
	// Default to upgrading all installed modules if neither list is specified.
	if len(upgradeJob.Spec.Modules) == 0 && len(upgradeJob.Spec.ModulesInstall) == 0 {
		args = append(args, "-u", "all")
	}

	ttl := int32(900)
	backoffLimit := int32(0)
	activeDeadline := int64(3600) // 1 hour max

	job := &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			GenerateName: fmt.Sprintf("%s-", upgradeJob.Name),
			Namespace:    upgradeJob.Namespace,
		},
		Spec: batchv1.JobSpec{
			BackoffLimit:            &backoffLimit,
			TTLSecondsAfterFinished: &ttl,
			ActiveDeadlineSeconds:   &activeDeadline,
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
							Name:    "upgrade",
							Image:   image,
							Command: []string{"/entrypoint.sh", "odoo"},
							Args:    args,
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

	if err := controllerutil.SetControllerReference(upgradeJob, job, r.Scheme); err != nil {
		return nil, err
	}
	return job, nil
}

func (r *OdooUpgradeJobReconciler) notifyWebhook(ctx context.Context, upgradeJob *bemadev1alpha1.OdooUpgradeJob, phase bemadev1alpha1.Phase) {
	log := logf.FromContext(ctx)
	wh := upgradeJob.Spec.Webhook

	token := wh.Token
	if token == "" && wh.SecretTokenSecretRef != nil {
		var secret corev1.Secret
		if err := r.Get(ctx, types.NamespacedName{
			Name:      wh.SecretTokenSecretRef.Name,
			Namespace: upgradeJob.Namespace,
		}, &secret); err == nil {
			token = string(secret.Data[wh.SecretTokenSecretRef.Key])
		}
	}

	payload, _ := json.Marshal(map[string]any{
		"upgradeJob":     upgradeJob.Name,
		"namespace":      upgradeJob.Namespace,
		"phase":          phase,
		"targetInstance": upgradeJob.Spec.OdooInstanceRef.Name,
		"modules":        upgradeJob.Spec.Modules,
		"modulesInstall": upgradeJob.Spec.ModulesInstall,
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

func (r *OdooUpgradeJobReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&bemadev1alpha1.OdooUpgradeJob{}).
		Owns(&batchv1.Job{}).
		Named("odooupgradejob").
		Complete(r)
}
