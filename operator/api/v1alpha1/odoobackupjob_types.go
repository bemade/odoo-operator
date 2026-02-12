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

import metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

// BackupDestination specifies where to store the backup artifact.
type BackupDestination struct {
	// s3 configures upload to an S3-compatible object store.
	S3 S3Config `json:"s3"`
}

// OdooBackupJobSpec defines the desired state of OdooBackupJob.
type OdooBackupJobSpec struct {
	// odooInstanceRef identifies the OdooInstance to back up.
	// +kubebuilder:validation:Required
	OdooInstanceRef OdooInstanceRef `json:"odooInstanceRef"`

	// destination specifies where the backup artifact is uploaded.
	// +kubebuilder:validation:Required
	Destination BackupDestination `json:"destination"`

	// format is the backup format.
	// +optional
	// +kubebuilder:default=zip
	Format BackupFormat `json:"format,omitempty"`

	// withFilestore includes the Odoo filestore in the backup when format is zip.
	// +optional
	// +kubebuilder:default=true
	WithFilestore bool `json:"withFilestore,omitempty"`

	// webhook is an optional callback invoked when the job completes or fails.
	// +optional
	Webhook *WebhookConfig `json:"webhook,omitempty"`
}

// OdooBackupJobStatus defines the observed state of OdooBackupJob.
type OdooBackupJobStatus struct {
	// phase is the current lifecycle phase of the backup job.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// jobName is the name of the Kubernetes Job performing the backup.
	// +optional
	JobName string `json:"jobName,omitempty"`

	// startTime is when the backup job began executing.
	// +optional
	StartTime *metav1.Time `json:"startTime,omitempty"`

	// completionTime is when the backup job finished.
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
// +kubebuilder:resource:shortName=backupjob
// +kubebuilder:printcolumn:name="Target",type=string,JSONPath=`.spec.odooInstanceRef.name`
// +kubebuilder:printcolumn:name="Format",type=string,JSONPath=`.spec.format`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// OdooBackupJob runs a one-shot backup of an OdooInstance to S3.
type OdooBackupJob struct {
	metav1.TypeMeta `json:",inline"`

	// +optional
	metav1.ObjectMeta `json:"metadata,omitzero"`

	// +required
	Spec OdooBackupJobSpec `json:"spec"`

	// +optional
	Status OdooBackupJobStatus `json:"status,omitzero"`
}

// +kubebuilder:object:root=true

// OdooBackupJobList contains a list of OdooBackupJob.
type OdooBackupJobList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitzero"`
	Items           []OdooBackupJob `json:"items"`
}

func init() {
	SchemeBuilder.Register(&OdooBackupJob{}, &OdooBackupJobList{})
}
