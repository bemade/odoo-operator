# Odoo Operator Helm Chart

A Helm chart for deploying the Odoo Operator on Kubernetes.

## Overview

This chart deploys:
- The Odoo Operator deployment
- Custom Resource Definitions (CRDs) for OdooInstance, OdooBackupJob, and OdooRestoreJob
- RBAC resources (ServiceAccount, ClusterRole, ClusterRoleBinding)
- ConfigMaps for operator configuration
- Standard Kubernetes Ingress support

## Prerequisites

- Kubernetes 1.20+
- Helm 3.x
- PostgreSQL database accessible from the cluster
- cert-manager (for TLS certificates)
- S3-compatible storage for backups (MinIO, AWS S3, etc.)

## Installation

### Quick Install

```bash
helm install odoo-operator ./odoo-operator \
  --namespace odoo-operator \
  --create-namespace
```

### With Custom Values

```bash
helm install odoo-operator ./odoo-operator \
  --namespace odoo-operator \
  --create-namespace \
  -f my-values.yaml
```

## Configuration

### Operator Settings

| Parameter | Description | Default |
|-----------|-------------|---------|
| `operator.image` | Operator container image | `registry.bemade.org/bemade/odoo-operator` |
| `operator.resources.requests.cpu` | CPU request | `50m` |
| `operator.resources.requests.memory` | Memory request | `64Mi` |
| `operator.resources.limits.cpu` | CPU limit | `250m` |
| `operator.resources.limits.memory` | Memory limit | `256Mi` |
| `operator.affinity` | Pod affinity rules | `{}` |
| `operator.tolerations` | Pod tolerations | `[]` |
| `operator.annotations` | Pod annotations | `{}` |

### Image Pull Secrets

The image pull secrets are used to authenticate with the container registry for pulling
Odoo instance images. Multiple secrets can be specified if needed, usually named after
the registry domain.

| Parameter | Description | Default |
|-----------|-------------|---------|
| `imagePullSecrets[].domain` | Registry domain | `registry.bemade.org` |
| `imagePullSecrets[].username` | Registry username | `username` |
| `imagePullSecrets[].password` | Registry password | `password` |

### PostgreSQL Cluster Configuration

The operator requires a Kubernetes secret containing PostgreSQL cluster configurations. **Exactly one cluster must have `default: true`** - this cluster is used for instances that don't specify a cluster and for migrating existing instances during upgrades.

#### 1. Create the secret

Create a file `clusters.yaml` with your cluster configurations:

```yaml
main:
  host: "postgres.postgres.svc.cluster.local"
  port: 5432
  adminUser: "postgres"
  adminPassword: "your-secure-password"
  default: true
analytics:
  host: "analytics-db.database.svc.cluster.local"
  port: 5432
  adminUser: "postgres"
  adminPassword: "another-secure-password"
  default: false
```

Then create the secret:

```bash
kubectl create secret generic postgres-clusters -n odoo-operator \
  --from-file=clusters.yaml=clusters.yaml
```

#### 2. Reference the secret in values

| Parameter | Description | Default |
|-----------|-------------|---------|
| `postgresClustersSecretRef.name` | Name of the secret containing clusters.yaml | *(required)* |

```yaml
postgresClustersSecretRef:
  name: "postgres-clusters"
```

OdooInstances can specify which cluster to use via `spec.database.cluster`. If not specified, the default cluster is used.

### Default Instance Settings

These defaults apply to new OdooInstance resources:

| Parameter | Description | Default |
|-----------|-------------|---------|
| `defaults.odooImage` | Default Odoo image | `odoo:18.0` |
| `defaults.storageClass` | Default storage class | `standard` |
| `defaults.resources.requests.cpu` | Default CPU request | `400m` |
| `defaults.resources.requests.memory` | Default memory request | `256Mi` |
| `defaults.resources.limits.cpu` | Default CPU limit | `2000m` |
| `defaults.resources.limits.memory` | Default memory limit | `4Gi` |
| `defaults.affinity` | Default pod affinity | `{}` |
| `defaults.tolerations` | Default pod tolerations | `[]` |
| `defaults.ingressClass` | Default ingress class for created Ingresses (optional) | *(unset; cluster default)* |

## Example values.yaml

```yaml
operator:
  image: registry.bemade.org/bemade/odoo-operator
  resources:
    requests:
      cpu: 100m
      memory: 128Mi
    limits:
      cpu: 500m
      memory: 512Mi

imagePullSecrets:
  - domain: registry.bemade.org
    username: myuser
    password: mypassword

postgresClustersSecretRef:
  name: "postgres-clusters"

defaults:
  odooImage: registry.bemade.org/bemade/odoo:18.0
  storageClass: "longhorn"
  resources:
    requests:
      cpu: 500m
      memory: 512Mi
    limits:
      cpu: 4000m
      memory: 8Gi
```

## Custom Resource Definitions

This chart installs three CRDs:

### OdooInstance

Defines an Odoo deployment with all its configuration.

```yaml
apiVersion: bemade.org/v1
kind: OdooInstance
metadata:
  name: my-odoo
spec:
  image: odoo:18.0
  replicas: 1
  adminPassword: "admin-password"
  ingress:
    hosts:
      - my-odoo.example.com
    issuer: letsencrypt-prod
  database:
    cluster: main  # Optional: specify which PostgreSQL cluster to use
```

### OdooBackupJob

Triggers a backup to S3-compatible storage.

```yaml
apiVersion: bemade.org/v1
kind: OdooBackupJob
metadata:
  name: backup-my-odoo
spec:
  odooInstanceRef:
    name: my-odoo
  format: zip
  destination:
    bucket: backups
    objectKey: my-odoo/backup.zip
    endpoint: https://minio.example.com
    s3CredentialsSecretRef:
      name: s3-credentials
```

### OdooRestoreJob

Restores a backup to an OdooInstance.

```yaml
apiVersion: bemade.org/v1
kind: OdooRestoreJob
metadata:
  name: restore-my-odoo
spec:
  odooInstanceRef:
    name: my-odoo
  source:
    type: s3
    s3:
      bucket: backups
      objectKey: my-odoo/backup.zip
      endpoint: https://minio.example.com
      s3CredentialsSecretRef:
        name: s3-credentials
  neutralize: true
```

## Upgrading

```bash
helm upgrade odoo-operator ./odoo-operator \
  --namespace odoo-operator \
  -f my-values.yaml
```

### Upgrading to v0.10.0+ (Multi-Cluster PostgreSQL)

Version 0.10.0 introduces multi-cluster PostgreSQL support. When upgrading from earlier versions:

1. **Required**: Create a secret containing your PostgreSQL cluster configuration (see above)
2. **Required**: Add `postgresClustersSecretRef.name` to your values file
3. The upgrade will automatically migrate existing OdooInstances to use the default cluster
4. You can then optionally update individual instances to use different clusters via `spec.database.cluster`

Example migration:
```bash
# 1. Create clusters.yaml file
cat > clusters.yaml << EOF
main:
  host: "postgres.database.svc.cluster.local"
  port: 5432
  adminUser: "postgres"
  adminPassword: "your-password"
  default: true
EOF

# 2. Create the secret
kubectl create secret generic postgres-clusters -n odoo-operator \
  --from-file=clusters.yaml=clusters.yaml

# 3. Upgrade with the secret reference
helm upgrade odoo-operator ./odoo-operator -n odoo-operator \
  --set postgresClustersSecretRef.name=postgres-clusters
```

**Note**: CRDs have `helm.sh/resource-policy: keep` annotation, so they won't be deleted on uninstall.

## Uninstalling

```bash
helm uninstall odoo-operator --namespace odoo-operator
```

To also remove CRDs (this will delete all OdooInstance resources!):

```bash
kubectl delete crd odooinstances.bemade.org
kubectl delete crd odoobackupjobs.bemade.org
kubectl delete crd odoorestorejobs.bemade.org
```

## Troubleshooting

### Check Operator Logs

```bash
kubectl logs -n odoo-operator deployment/odoo-operator -f
```

### Check OdooInstance Status

```bash
kubectl get odooinstances -A
kubectl describe odooinstance my-odoo
```

### Check Backup/Restore Jobs

```bash
kubectl get odoobackupjobs -A
kubectl get odoorestorejobs -A
kubectl get jobs -n <namespace>
```

## License

LGPLv3