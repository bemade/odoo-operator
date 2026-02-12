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

package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// RestoreSourceType identifies the origin of the backup artifact.
// +kubebuilder:validation:Enum=s3;odoo
type RestoreSourceType string

const (
	// RestoreSourceTypeS3 restores from an S3-compatible object store.
	RestoreSourceTypeS3 RestoreSourceType = "s3"
	// RestoreSourceTypeOdoo downloads a backup directly from a running Odoo instance.
	RestoreSourceTypeOdoo RestoreSourceType = "odoo"
)

// OdooLiveSource downloads a backup from a running Odoo instance via its web backup endpoint.
type OdooLiveSource struct {
	// url is the base URL of the source Odoo instance (e.g. "https://src.example.com").
	URL string `json:"url"`

	// sourceDatabase is the name of the database to download from the source.
	// +optional
	SourceDatabase string `json:"sourceDatabase,omitempty"`

	// masterPasswordSecretRef references a Secret whose key contains the Odoo master password.
	// +optional
	MasterPasswordSecretRef *corev1.SecretKeySelector `json:"masterPasswordSecretRef,omitempty"`
}

// RestoreSource defines where to download the backup artifact from.
type RestoreSource struct {
	// type identifies the source backend.
	Type RestoreSourceType `json:"type"`

	// s3 configures an S3-compatible source. Required when type is "s3".
	// +optional
	S3 *S3Config `json:"s3,omitempty"`

	// odoo configures a live Odoo instance source. Required when type is "odoo".
	// +optional
	Odoo *OdooLiveSource `json:"odoo,omitempty"`
}

// OdooRestoreJobSpec defines the desired state of OdooRestoreJob.
type OdooRestoreJobSpec struct {
	// odooInstanceRef identifies the OdooInstance to restore into.
	// +kubebuilder:validation:Required
	OdooInstanceRef OdooInstanceRef `json:"odooInstanceRef"`

	// source describes where to download the backup artifact from.
	// +kubebuilder:validation:Required
	Source RestoreSource `json:"source"`

	// format is the backup format of the artifact being restored.
	// +optional
	// +kubebuilder:default=zip
	Format BackupFormat `json:"format,omitempty"`

	// neutralize runs odoo-neutralize after restore to anonymise data.
	// Recommended when restoring production data to a non-production environment.
	// +optional
	// +kubebuilder:default=true
	Neutralize bool `json:"neutralize,omitempty"`

	// webhook is an optional callback invoked when the job completes or fails.
	// +optional
	Webhook *WebhookConfig `json:"webhook,omitempty"`
}

// OdooRestoreJobStatus defines the observed state of OdooRestoreJob.
type OdooRestoreJobStatus struct {
	// phase is the current lifecycle phase of the restore job.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// jobName is the name of the Kubernetes Job performing the restore.
	// +optional
	JobName string `json:"jobName,omitempty"`

	// startTime is when the restore job began executing.
	// +optional
	StartTime *metav1.Time `json:"startTime,omitempty"`

	// completionTime is when the restore job finished.
	// +optional
	CompletionTime *metav1.Time `json:"completionTime,omitempty"`

	// message is a human-readable description of the current status.
	// +optional
	Message string `json:"message,omitempty"`

	// conditions represent the detailed state of this resource.
	// +listType=map
	// +listMapKey=type
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=restore
// +kubebuilder:printcolumn:name="Target",type=string,JSONPath=`.spec.odooInstanceRef.name`
// +kubebuilder:printcolumn:name="Source",type=string,JSONPath=`.spec.source.type`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// OdooRestoreJob restores an OdooInstance from a backup artifact.
type OdooRestoreJob struct {
	metav1.TypeMeta `json:",inline"`

	// +optional
	metav1.ObjectMeta `json:"metadata,omitzero"`

	// +required
	Spec OdooRestoreJobSpec `json:"spec"`

	// +optional
	Status OdooRestoreJobStatus `json:"status,omitzero"`
}

// +kubebuilder:object:root=true

// OdooRestoreJobList contains a list of OdooRestoreJob.
type OdooRestoreJobList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitzero"`
	Items           []OdooRestoreJob `json:"items"`
}

func init() {
	SchemeBuilder.Register(&OdooRestoreJob{}, &OdooRestoreJobList{})
}
