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

// OdooInstanceRef is a reference to an OdooInstance resource.
type OdooInstanceRef struct {
	// name of the OdooInstance.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// namespace of the OdooInstance. Defaults to the same namespace as this resource.
	// +optional
	Namespace string `json:"namespace,omitempty"`
}

// WebhookConfig defines an optional webhook callback for job completion notifications.
type WebhookConfig struct {
	// url to POST status updates to.
	// +kubebuilder:validation:Required
	URL string `json:"url"`

	// token is a bearer token included in the Authorization header.
	// +optional
	Token string `json:"token,omitempty"`

	// secretTokenSecretRef references a Secret containing the bearer token,
	// as an alternative to specifying it inline.
	// +optional
	SecretTokenSecretRef *corev1.SecretKeySelector `json:"secretTokenSecretRef,omitempty"`
}

// OdooInitJobSpec defines the desired state of OdooInitJob.
type OdooInitJobSpec struct {
	// odooInstanceRef identifies the OdooInstance to initialize.
	// +kubebuilder:validation:Required
	OdooInstanceRef OdooInstanceRef `json:"odooInstanceRef"`

	// modules to install during initialization. Defaults to ["base"].
	// +optional
	// +kubebuilder:default={"base"}
	Modules []string `json:"modules,omitempty"`

	// webhook is an optional callback invoked when the job completes or fails.
	// +optional
	Webhook *WebhookConfig `json:"webhook,omitempty"`
}

// Phase represents the lifecycle state of a job resource.
// +kubebuilder:validation:Enum=Pending;Running;Completed;Failed
type Phase string

const (
	PhasePending   Phase = "Pending"
	PhaseRunning   Phase = "Running"
	PhaseCompleted Phase = "Completed"
	PhaseFailed    Phase = "Failed"
)

// OdooInitJobStatus defines the observed state of OdooInitJob.
type OdooInitJobStatus struct {
	// phase is the current lifecycle phase of the init job.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// jobName is the name of the Kubernetes Job performing the initialization.
	// +optional
	JobName string `json:"jobName,omitempty"`

	// startTime is when the initialization job began executing.
	// +optional
	StartTime *metav1.Time `json:"startTime,omitempty"`

	// completionTime is when the initialization job finished.
	// +optional
	CompletionTime *metav1.Time `json:"completionTime,omitempty"`

	// message is a human-readable description of the current status.
	// +optional
	Message string `json:"message,omitempty"`

	// conditions represent the detailed state of this resource using
	// standard Kubernetes condition conventions.
	// +listType=map
	// +listMapKey=type
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=initjob
// +kubebuilder:printcolumn:name="Target",type=string,JSONPath=`.spec.odooInstanceRef.name`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// OdooInitJob runs a one-shot database initialisation job against an OdooInstance.
type OdooInitJob struct {
	metav1.TypeMeta `json:",inline"`

	// +optional
	metav1.ObjectMeta `json:"metadata,omitzero"`

	// +required
	Spec OdooInitJobSpec `json:"spec"`

	// +optional
	Status OdooInitJobStatus `json:"status,omitzero"`
}

// +kubebuilder:object:root=true

// OdooInitJobList contains a list of OdooInitJob.
type OdooInitJobList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitzero"`
	Items           []OdooInitJob `json:"items"`
}

func init() {
	SchemeBuilder.Register(&OdooInitJob{}, &OdooInitJobList{})
}
