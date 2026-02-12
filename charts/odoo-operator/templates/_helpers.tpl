{{/*
v1LegacyVersion injects a served-but-not-stored v1 version entry into a CRD's
versions list.  This is required for Helm upgrades on clusters that previously
ran the Kopf Python operator, which stored objects as bemade.org/v1.  Kubernetes
refuses to remove a version from spec.versions while it is still listed in
status.storedVersions, so we keep v1 served (with an open schema) until all
objects have been rewritten in v1alpha1 storage.
*/}}
{{- define "odoo-operator.v1LegacyVersion" -}}
- additionalPrinterColumns:
  - jsonPath: .spec.odooInstanceRef.name
    name: Target
    type: string
  - jsonPath: .status.phase
    name: Phase
    type: string
  - jsonPath: .metadata.creationTimestamp
    name: Age
    type: date
  deprecated: true
  deprecationWarning: "bemade.org/v1 is deprecated; migrate to bemade.org/v1alpha1"
  name: v1
  schema:
    openAPIV3Schema:
      type: object
      x-kubernetes-preserve-unknown-fields: true
  served: false
  storage: false
  subresources:
    status: {}
{{- end }}

{{/*
v1LegacyVersionWithScale is identical to v1LegacyVersion but also exposes the
scale subresource, required for OdooInstance which supports kubectl scale.
*/}}
{{- define "odoo-operator.v1LegacyVersionWithScale" -}}
- additionalPrinterColumns:
  - jsonPath: .spec.image
    name: Image
    type: string
  - jsonPath: .status.readyReplicas
    name: Replicas
    type: string
  - jsonPath: .status.phase
    name: Phase
    type: string
  - jsonPath: .status.url
    name: URL
    type: string
  - jsonPath: .metadata.creationTimestamp
    name: Age
    type: date
  deprecated: true
  deprecationWarning: "bemade.org/v1 is deprecated; migrate to bemade.org/v1alpha1"
  name: v1
  schema:
    openAPIV3Schema:
      type: object
      x-kubernetes-preserve-unknown-fields: true
  served: false
  storage: false
  subresources:
    scale:
      specReplicasPath: .spec.replicas
      statusReplicasPath: .status.readyReplicas
    status: {}
{{- end }}
