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

// IngressSpec defines how the OdooInstance should be exposed via an Ingress resource.
type IngressSpec struct {
	// hosts are the fully qualified domain names at which this instance is reachable.
	// +kubebuilder:validation:MinItems=1
	Hosts []string `json:"hosts"`

	// issuer is the name of the cert-manager ClusterIssuer used to issue the TLS certificate.
	// Defaults to the operator-level default when not set.
	// +optional
	Issuer string `json:"issuer,omitempty"`

	// class is the IngressClass to use (e.g. "nginx", "traefik").
	// +optional
	Class *string `json:"class,omitempty"`
}

// FilestoreSpec defines persistent storage for the Odoo filestore.
type FilestoreSpec struct {
	// storageSize is the PVC size requested for the filestore (e.g. "10Gi").
	// Defaults to the operator-level default (typically "2Gi").
	// +optional
	StorageSize string `json:"storageSize,omitempty"`

	// storageClass is the StorageClass to use for the filestore PVC.
	// Defaults to the operator-level default (typically "standard").
	// +optional
	StorageClass string `json:"storageClass,omitempty"`
}

// DatabaseSpec identifies which PostgreSQL cluster to use for this instance.
type DatabaseSpec struct {
	// cluster is the name of the PostgreSQL cluster from the postgres-clusters secret.
	// Defaults to the cluster marked as default.
	// +optional
	Cluster string `json:"cluster,omitempty"`
}

// DeploymentStrategyType specifies the update strategy for the Odoo Deployment.
// +kubebuilder:validation:Enum=Recreate;RollingUpdate
type DeploymentStrategyType string

const (
	DeploymentStrategyRecreate      DeploymentStrategyType = "Recreate"
	DeploymentStrategyRollingUpdate DeploymentStrategyType = "RollingUpdate"
)

// RollingUpdateSpec configures the RollingUpdate deployment strategy parameters.
type RollingUpdateSpec struct {
	// maxUnavailable is the maximum number of pods that can be unavailable during the update.
	// +optional
	// +kubebuilder:default="25%"
	MaxUnavailable string `json:"maxUnavailable,omitempty"`

	// maxSurge is the maximum number of additional pods that can be created during the update.
	// +optional
	// +kubebuilder:default="25%"
	MaxSurge string `json:"maxSurge,omitempty"`
}

// StrategySpec defines the Deployment update strategy.
type StrategySpec struct {
	// type is the deployment strategy type.
	// +optional
	// +kubebuilder:default=Recreate
	Type DeploymentStrategyType `json:"type,omitempty"`

	// rollingUpdate configures rolling update parameters. Only applicable when Type is RollingUpdate.
	// +optional
	RollingUpdate *RollingUpdateSpec `json:"rollingUpdate,omitempty"`
}

// OdooWebhookConfig defines an optional webhook callback for status change notifications.
// TODO: unify with the WebhookConfig in v1alpha1 once a shared package is established.
type OdooWebhookConfig struct {
	// url to POST status updates to when the instance phase changes.
	// +kubebuilder:validation:Required
	URL string `json:"url"`
}

// ProbesSpec configures the HTTP health check paths for Kubernetes probes.
type ProbesSpec struct {
	// startupPath is the HTTP path used for the startup probe.
	// +optional
	// +kubebuilder:default="/web/health"
	StartupPath string `json:"startupPath,omitempty"`

	// livenessPath is the HTTP path used for the liveness probe.
	// +optional
	// +kubebuilder:default="/web/health"
	LivenessPath string `json:"livenessPath,omitempty"`

	// readinessPath is the HTTP path used for the readiness probe.
	// Use /health/ready if the health_check_k8s module is installed for deep checks (DB + filestore).
	// +optional
	// +kubebuilder:default="/web/health"
	ReadinessPath string `json:"readinessPath,omitempty"`
}

// OdooInstanceSpec defines the desired state of OdooInstance.
type OdooInstanceSpec struct {
	// image is the Odoo container image to use (e.g. "odoo:18.0").
	// Defaults to the operator-level default when not specified.
	// +optional
	Image string `json:"image,omitempty"`

	// imagePullSecret is the name of the Secret containing registry credentials.
	// +optional
	ImagePullSecret string `json:"imagePullSecret,omitempty"`

	// adminPassword is the Odoo database administrator password.
	AdminPassword string `json:"adminPassword"`

	// replicas is the number of Odoo pod replicas to run. Set to 0 to stop the instance.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	Replicas int32 `json:"replicas"`

	// ingress configures the Ingress resource for this instance.
	Ingress IngressSpec `json:"ingress"`

	// resources sets CPU and memory requests and limits for the Odoo container.
	// +optional
	Resources *corev1.ResourceRequirements `json:"resources,omitempty"`

	// filestore configures persistent storage for the Odoo filestore.
	// +optional
	Filestore *FilestoreSpec `json:"filestore,omitempty"`

	// configOptions are additional key-value pairs appended to odoo.conf.
	// +optional
	ConfigOptions map[string]string `json:"configOptions,omitempty"`

	// database identifies the PostgreSQL cluster to use for this instance.
	// +optional
	Database *DatabaseSpec `json:"database,omitempty"`

	// strategy defines the Deployment update strategy.
	// +optional
	Strategy *StrategySpec `json:"strategy,omitempty"`

	// webhook is an optional callback invoked when the instance phase changes.
	// +optional
	Webhook *OdooWebhookConfig `json:"webhook,omitempty"`

	// probes configures the HTTP health check paths for Kubernetes liveness, readiness, and startup probes.
	// +optional
	Probes *ProbesSpec `json:"probes,omitempty"`

	// affinity defines node affinity/anti-affinity rules for the Odoo pod.
	// Defaults to the operator-level default when not set.
	// +optional
	Affinity *corev1.Affinity `json:"affinity,omitempty"`

	// tolerations define tolerations for the Odoo pod.
	// Defaults to the operator-level default when not set.
	// +optional
	Tolerations []corev1.Toleration `json:"tolerations,omitempty"`
}

// OdooInstancePhase represents the lifecycle state of an OdooInstance.
// +kubebuilder:validation:Enum=Provisioning;Uninitialized;Initializing;InitFailed;Starting;Running;Degraded;Stopped;Upgrading;UpgradeFailed;Restoring;RestoreFailed;Error
type OdooInstancePhase string

const (
	OdooInstancePhaseProvisioning  OdooInstancePhase = "Provisioning"
	OdooInstancePhaseUninitialized OdooInstancePhase = "Uninitialized"
	OdooInstancePhaseInitializing  OdooInstancePhase = "Initializing"
	OdooInstancePhaseInitFailed    OdooInstancePhase = "InitFailed"
	OdooInstancePhaseStarting      OdooInstancePhase = "Starting"
	OdooInstancePhaseRunning       OdooInstancePhase = "Running"
	OdooInstancePhaseDegraded      OdooInstancePhase = "Degraded"
	OdooInstancePhaseStopped       OdooInstancePhase = "Stopped"
	OdooInstancePhaseUpgrading     OdooInstancePhase = "Upgrading"
	OdooInstancePhaseUpgradeFailed OdooInstancePhase = "UpgradeFailed"
	OdooInstancePhaseRestoring     OdooInstancePhase = "Restoring"
	OdooInstancePhaseRestoreFailed OdooInstancePhase = "RestoreFailed"
	OdooInstancePhaseError         OdooInstancePhase = "Error"
)

// OdooInstanceStatus defines the observed state of OdooInstance.
type OdooInstanceStatus struct {
	// phase is the current lifecycle phase of the instance.
	// +optional
	Phase OdooInstancePhase `json:"phase,omitempty"`

	// message is a human-readable description of the current status.
	// +optional
	Message string `json:"message,omitempty"`

	// ready is true when the instance is fully functional and all pods are ready.
	// +optional
	Ready bool `json:"ready,omitempty"`

	// readyReplicas is the number of Odoo pods currently ready.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// dbInitialized is true once a completed OdooInitJob has been observed for this instance.
	// Once true, it is never cleared — even if the InitJob is deleted.
	// +optional
	DBInitialized bool `json:"dbInitialized,omitempty"`

	// url is the primary URL at which the instance is reachable.
	// +optional
	URL string `json:"url,omitempty"`

	// lastBackup is the timestamp of the most recent successful backup.
	// +optional
	LastBackup *metav1.Time `json:"lastBackup,omitempty"`

	// conditions represent the detailed state of this resource using
	// standard Kubernetes condition conventions.
	// +listType=map
	// +listMapKey=type
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:subresource:scale:specpath=.spec.replicas,statuspath=.status.readyReplicas
// +kubebuilder:resource:shortName=odoo
// +kubebuilder:printcolumn:name="Image",type=string,JSONPath=`.spec.image`
// +kubebuilder:printcolumn:name="Replicas",type=string,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="URL",type=string,JSONPath=`.status.url`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// OdooInstance is the Schema for the odooinstances API.
type OdooInstance struct {
	metav1.TypeMeta `json:",inline"`

	// +optional
	metav1.ObjectMeta `json:"metadata,omitzero"`

	// +required
	Spec OdooInstanceSpec `json:"spec"`

	// +optional
	Status OdooInstanceStatus `json:"status,omitzero"`
}

// +kubebuilder:object:root=true

// OdooInstanceList contains a list of OdooInstance.
type OdooInstanceList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitzero"`
	Items           []OdooInstance `json:"items"`
}

func init() {
	SchemeBuilder.Register(&OdooInstance{}, &OdooInstanceList{})
}
