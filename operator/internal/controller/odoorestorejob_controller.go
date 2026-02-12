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

// downloadScript downloads the backup artifact from S3 to /mnt/restore/.
const downloadScript = `
set -ex
MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR" /mnt/restore

MC_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && MC_INSECURE="--insecure"

mc $MC_INSECURE alias set src "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_INSECURE cp "src/$S3_BUCKET/$S3_KEY" "/mnt/restore/$LOCAL_FILENAME"
echo "Download complete: /mnt/restore/$LOCAL_FILENAME"
`

// restoreScript restores the backup artifact and optionally neutralises the DB.
const restoreScript = `
set -ex
export PGPASSWORD=$PASSWORD

echo "=== Dropping existing database if present ==="
psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres \
  -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB_NAME' AND pid <> pg_backend_pid();" || true
psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres \
  -c "DROP DATABASE IF EXISTS \"$DB_NAME\";" || true

echo "=== Restoring from $BACKUP_FORMAT backup ==="
case "$BACKUP_FORMAT" in
  zip)
    odoo db \
      --db_host "$HOST" --db_port "$PORT" \
      --db_user "$USER" --db_password "$PASSWORD" \
      restore "$DB_NAME" "/mnt/restore/$LOCAL_FILENAME"
    ;;
  dump)
    psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres -c "CREATE DATABASE \"$DB_NAME\";"
    pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
      --no-owner --no-privileges "/mnt/restore/$LOCAL_FILENAME"
    ;;
  *)
    psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres -c "CREATE DATABASE \"$DB_NAME\";"
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" < "/mnt/restore/$LOCAL_FILENAME"
    ;;
esac

if [ "$NEUTRALIZE" = "true" ]; then
  echo "=== Neutralising database ==="
  odoo neutralize \
    --db_host "$HOST" --db_port "$PORT" \
    --db_user "$USER" --db_password "$PASSWORD" \
    --database "$DB_NAME" || {
    echo "Neutralisation failed — dropping database for safety"
    psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres \
      -c "DROP DATABASE IF EXISTS \"$DB_NAME\";"
    exit 1
  }
fi

echo "=== Restore complete ==="
`

// OdooRestoreJobReconciler reconciles a OdooRestoreJob object.
type OdooRestoreJobReconciler struct {
	client.Client
	Scheme     *runtime.Scheme
	HTTPClient *http.Client
}

// +kubebuilder:rbac:groups=bemade.org,resources=odoorestorejobs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=bemade.org,resources=odoorestorejobs/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=bemade.org,resources=odoorestorejobs/finalizers,verbs=update
// +kubebuilder:rbac:groups=bemade.org,resources=odooinstances,verbs=get;list;watch
// +kubebuilder:rbac:groups=batch,resources=jobs,verbs=get;list;watch;create;delete
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;patch
// +kubebuilder:rbac:groups=core,resources=secrets,verbs=get

func (r *OdooRestoreJobReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var restoreJob bemadev1alpha1.OdooRestoreJob
	if err := r.Get(ctx, req.NamespacedName, &restoreJob); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	if restoreJob.Status.Phase == bemadev1alpha1.PhaseCompleted ||
		restoreJob.Status.Phase == bemadev1alpha1.PhaseFailed {
		return ctrl.Result{}, nil
	}

	if restoreJob.Status.JobName == "" {
		return r.startJob(ctx, &restoreJob)
	}

	log.Info("checking restore job status", "job", restoreJob.Status.JobName)
	return r.syncJobStatus(ctx, &restoreJob)
}

func (r *OdooRestoreJobReconciler) startJob(ctx context.Context, restoreJob *bemadev1alpha1.OdooRestoreJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	instanceNS := restoreJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = restoreJob.Namespace
	}
	instanceName := restoreJob.Spec.OdooInstanceRef.Name

	var odooInstance bemadev1alpha1.OdooInstance
	if err := r.Get(ctx, types.NamespacedName{Name: instanceName, Namespace: instanceNS}, &odooInstance); err != nil {
		if errors.IsNotFound(err) {
			return r.setFailed(ctx, restoreJob, fmt.Sprintf("OdooInstance %s not found", instanceName))
		}
		return ctrl.Result{}, err
	}

	// Scale down before restoring to prevent writes during restore.
	if err := scaleDeployment(ctx, r.Client, instanceName, instanceNS, 0); err != nil {
		log.Error(err, "failed to scale down deployment — proceeding anyway", "instance", instanceName)
	}

	job, err := r.buildRestoreJob(restoreJob, &odooInstance)
	if err != nil {
		return r.setFailed(ctx, restoreJob, fmt.Sprintf("failed to build job: %v", err))
	}

	if err := r.Create(ctx, job); err != nil {
		return ctrl.Result{}, fmt.Errorf("creating restore job: %w", err)
	}

	log.Info("created restore job", "job", job.Name)

	patch := client.MergeFrom(restoreJob.DeepCopy())
	restoreJob.Status.Phase = bemadev1alpha1.PhaseRunning
	restoreJob.Status.JobName = job.Name
	now := metav1.Now()
	restoreJob.Status.StartTime = &now
	if err := r.Status().Patch(ctx, restoreJob, patch); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating status to Running: %w", err)
	}

	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

func (r *OdooRestoreJobReconciler) syncJobStatus(ctx context.Context, restoreJob *bemadev1alpha1.OdooRestoreJob) (ctrl.Result, error) {
	log := logf.FromContext(ctx)

	var job batchv1.Job
	if err := r.Get(ctx, types.NamespacedName{Name: restoreJob.Status.JobName, Namespace: restoreJob.Namespace}, &job); err != nil {
		if errors.IsNotFound(err) {
			log.Info("job not found, may have been deleted", "job", restoreJob.Status.JobName)
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	if job.Status.Succeeded > 0 {
		log.Info("restore job succeeded")
		return ctrl.Result{}, r.finalise(ctx, restoreJob, bemadev1alpha1.PhaseCompleted, "")
	}
	if job.Status.Failed > 0 {
		log.Info("restore job failed")
		return ctrl.Result{}, r.finalise(ctx, restoreJob, bemadev1alpha1.PhaseFailed, "restore job failed")
	}

	return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
}

func (r *OdooRestoreJobReconciler) finalise(ctx context.Context, restoreJob *bemadev1alpha1.OdooRestoreJob, phase bemadev1alpha1.Phase, message string) error {
	patch := client.MergeFrom(restoreJob.DeepCopy())
	restoreJob.Status.Phase = phase
	restoreJob.Status.Message = message
	now := metav1.Now()
	restoreJob.Status.CompletionTime = &now
	if err := r.Status().Patch(ctx, restoreJob, patch); err != nil {
		return fmt.Errorf("updating terminal status: %w", err)
	}

	instanceNS := restoreJob.Spec.OdooInstanceRef.Namespace
	if instanceNS == "" {
		instanceNS = restoreJob.Namespace
	}
	if err := r.scaleInstanceBackUp(ctx, restoreJob.Spec.OdooInstanceRef.Name, instanceNS); err != nil {
		logf.FromContext(ctx).Error(err, "failed to scale instance back up")
	}

	if restoreJob.Spec.Webhook != nil {
		r.notifyWebhook(ctx, restoreJob, phase)
	}

	return nil
}

func (r *OdooRestoreJobReconciler) scaleInstanceBackUp(ctx context.Context, instanceName, instanceNS string) error {
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

func (r *OdooRestoreJobReconciler) setFailed(ctx context.Context, restoreJob *bemadev1alpha1.OdooRestoreJob, message string) (ctrl.Result, error) {
	patch := client.MergeFrom(restoreJob.DeepCopy())
	restoreJob.Status.Phase = bemadev1alpha1.PhaseFailed
	restoreJob.Status.Message = message
	return ctrl.Result{}, r.Status().Patch(ctx, restoreJob, patch)
}

func (r *OdooRestoreJobReconciler) buildRestoreJob(restoreJob *bemadev1alpha1.OdooRestoreJob, odooInstance *bemadev1alpha1.OdooInstance) (*batchv1.Job, error) {
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
	dbConfigSecretName := fmt.Sprintf("%s-db-config", instanceName)
	dbName := fmt.Sprintf("odoo_%s", sanitiseUID(instanceUID))

	format := string(restoreJob.Spec.Format)
	if format == "" {
		format = "zip"
	}

	neutralize := "true"
	if !restoreJob.Spec.Neutralize {
		neutralize = "false"
	}

	src := restoreJob.Spec.Source

	// Determine local filename from S3 key or a default.
	localFilename := "restore-artifact"
	if src.S3 != nil && src.S3.ObjectKey != "" {
		localFilename = src.S3.ObjectKey
	}

	sharedMount := corev1.VolumeMount{Name: "restore", MountPath: "/mnt/restore"}

	dbEnv := []corev1.EnvVar{
		{Name: "DB_NAME", Value: dbName},
		{Name: "BACKUP_FORMAT", Value: format},
		{Name: "NEUTRALIZE", Value: neutralize},
		{Name: "LOCAL_FILENAME", Value: localFilename},
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
	}

	// Build init containers: download first, then restore.
	var initContainers []corev1.Container

	if src.Type == bemadev1alpha1.RestoreSourceTypeS3 && src.S3 != nil {
		insecureVal := "false"
		if src.S3.Insecure {
			insecureVal = "true"
		}
		dlEnv := []corev1.EnvVar{
			{Name: "S3_BUCKET", Value: src.S3.Bucket},
			{Name: "S3_KEY", Value: src.S3.ObjectKey},
			{Name: "S3_ENDPOINT", Value: src.S3.Endpoint},
			{Name: "S3_INSECURE", Value: insecureVal},
			{Name: "LOCAL_FILENAME", Value: localFilename},
			{Name: "MC_CONFIG_DIR", Value: "/tmp/.mc"},
		}
		if src.S3.CredentialsSecretRef != nil {
			dlEnv = append(dlEnv,
				corev1.EnvVar{
					Name: "AWS_ACCESS_KEY_ID",
					ValueFrom: &corev1.EnvVarSource{
						SecretKeyRef: &corev1.SecretKeySelector{
							LocalObjectReference: corev1.LocalObjectReference{Name: src.S3.CredentialsSecretRef.Name},
							Key:                  "accessKey",
						},
					},
				},
				corev1.EnvVar{
					Name: "AWS_SECRET_ACCESS_KEY",
					ValueFrom: &corev1.EnvVarSource{
						SecretKeyRef: &corev1.SecretKeySelector{
							LocalObjectReference: corev1.LocalObjectReference{Name: src.S3.CredentialsSecretRef.Name},
							Key:                  "secretKey",
						},
					},
				},
			)
		}
		initContainers = append(initContainers, corev1.Container{
			Name:         "downloader",
			Image:        "quay.io/minio/mc:latest",
			Command:      []string{"/bin/sh", "-c", downloadScript},
			Env:          dlEnv,
			VolumeMounts: []corev1.VolumeMount{sharedMount},
		})
	}

	initContainers = append(initContainers, corev1.Container{
		Name:    "restore",
		Image:   image,
		Command: []string{"/bin/sh", "-c", restoreScript},
		Env:     dbEnv,
		VolumeMounts: []corev1.VolumeMount{
			{Name: "filestore", MountPath: "/var/lib/odoo"},
			{Name: "odoo-conf", MountPath: "/etc/odoo"},
			sharedMount,
		},
	})

	ttl := int32(900)
	backoffLimit := int32(0)
	activeDeadline := int64(3600) // 1 hour

	job := &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			GenerateName: fmt.Sprintf("%s-", restoreJob.Name),
			Namespace:    restoreJob.Namespace,
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
						{
							Name:         "restore",
							VolumeSource: corev1.VolumeSource{EmptyDir: &corev1.EmptyDirVolumeSource{}},
						},
					},
					InitContainers: initContainers,
					// Sentinel container: exits immediately once all init containers succeed.
					Containers: []corev1.Container{
						{
							Name:    "done",
							Image:   "busybox:latest",
							Command: []string{"/bin/sh", "-c", "echo restore complete"},
						},
					},
				},
			},
		},
	}

	if err := controllerutil.SetControllerReference(restoreJob, job, r.Scheme); err != nil {
		return nil, err
	}
	return job, nil
}

func (r *OdooRestoreJobReconciler) notifyWebhook(ctx context.Context, restoreJob *bemadev1alpha1.OdooRestoreJob, phase bemadev1alpha1.Phase) {
	log := logf.FromContext(ctx)
	wh := restoreJob.Spec.Webhook

	token := wh.Token
	if token == "" && wh.SecretTokenSecretRef != nil {
		var secret corev1.Secret
		if err := r.Get(ctx, types.NamespacedName{
			Name:      wh.SecretTokenSecretRef.Name,
			Namespace: restoreJob.Namespace,
		}, &secret); err == nil {
			token = string(secret.Data[wh.SecretTokenSecretRef.Key])
		}
	}

	payload, _ := json.Marshal(map[string]any{
		"restoreJob":     restoreJob.Name,
		"namespace":      restoreJob.Namespace,
		"phase":          phase,
		"targetInstance": restoreJob.Spec.OdooInstanceRef.Name,
		"sourceType":     restoreJob.Spec.Source.Type,
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

func (r *OdooRestoreJobReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&bemadev1alpha1.OdooRestoreJob{}).
		Owns(&batchv1.Job{}).
		Named("odoorestorejob").
		Complete(r)
}
