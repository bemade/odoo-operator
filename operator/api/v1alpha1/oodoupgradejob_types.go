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

// OdooUpgradeJobSpec defines the desired state of OdooUpgradeJob.
type OdooUpgradeJobSpec struct {
	// odooInstanceRef identifies the OdooInstance to upgrade.
	// +kubebuilder:validation:Required
	OdooInstanceRef OdooInstanceRef `json:"odooInstanceRef"`

	// modules is the list of already-installed modules to upgrade (-u flag).
	// +optional
	Modules []string `json:"modules,omitempty"`

	// modulesInstall is the list of new modules to install (-i flag).
	// +optional
	ModulesInstall []string `json:"modulesInstall,omitempty"`

	// scheduledTime is an optional time at which the upgrade should run.
	// If set and in the future the controller will requeue until the time arrives.
	// +optional
	ScheduledTime *metav1.Time `json:"scheduledTime,omitempty"`

	// webhook is an optional callback invoked when the job completes or fails.
	// +optional
	Webhook *WebhookConfig `json:"webhook,omitempty"`
}

// OdooUpgradeJobStatus defines the observed state of OdooUpgradeJob.
type OdooUpgradeJobStatus struct {
	// phase is the current lifecycle phase of the upgrade job.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// jobName is the name of the Kubernetes Job performing the upgrade.
	// +optional
	JobName string `json:"jobName,omitempty"`

	// startTime is when the upgrade job began executing.
	// +optional
	StartTime *metav1.Time `json:"startTime,omitempty"`

	// completionTime is when the upgrade job finished.
	// +optional
	CompletionTime *metav1.Time `json:"completionTime,omitempty"`

	// message is a human-readable description of the current status.
	// +optional
	Message string `json:"message,omitempty"`

	// originalReplicas records the replica count before scale-down so it can be
	// restored after the upgrade completes.
	// +optional
	OriginalReplicas int32 `json:"originalReplicas,omitempty"`

	// conditions represent the detailed state of this resource.
	// +listType=map
	// +listMapKey=type
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=upgradejob
// +kubebuilder:printcolumn:name="Target",type=string,JSONPath=`.spec.odooInstanceRef.name`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// OdooUpgradeJob runs a one-shot module upgrade against an OdooInstance.
type OdooUpgradeJob struct {
	metav1.TypeMeta `json:",inline"`

	// +optional
	metav1.ObjectMeta `json:"metadata,omitzero"`

	// +required
	Spec OdooUpgradeJobSpec `json:"spec"`

	// +optional
	Status OdooUpgradeJobStatus `json:"status,omitzero"`
}

// +kubebuilder:object:root=true

// OdooUpgradeJobList contains a list of OdooUpgradeJob.
type OdooUpgradeJobList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitzero"`
	Items           []OdooUpgradeJob `json:"items"`
}

func init() {
	SchemeBuilder.Register(&OdooUpgradeJob{}, &OdooUpgradeJobList{})
}
