# Odoo Operator

A Kubernetes operator for managing Odoo instances at scale. This operator automates the deployment, backup, restore, and lifecycle management of Odoo applications on Kubernetes.

## Features

- **Declarative Odoo Management**: Define Odoo instances as Kubernetes Custom Resources
- **Automated Database Provisioning**: Automatic PostgreSQL database and user creation
- **Backup to S3/MinIO**: Schedule and manage backups with S3-compatible storage
- **Restore Operations**: Restore from S3 backups or live Odoo instances
- **Rolling Updates**: Seamless image updates with zero-downtime deployments
- **Module Upgrades**: Trigger Odoo module upgrades via CR spec changes
- **Webhook Notifications**: Get notified of status changes via webhooks

## Architecture

The operator watches three Custom Resource Definitions (CRDs):

| CRD | Purpose |
|-----|---------|
| `OdooInstance` | Defines an Odoo deployment (image, replicas, ingress, resources) |
| `OdooBackupJob` | Triggers a backup of an OdooInstance to S3 |
| `OdooRestoreJob` | Restores a backup to an OdooInstance |

## Quick Start

### Prerequisites

- Kubernetes cluster (1.20+)
- Helm 3.x
- PostgreSQL database accessible from the cluster
- S3-compatible storage (MinIO, AWS S3, etc.) for backups

### Installation

```bash
# Add the Helm repository
helm repo add bemade https://bemade.github.io/helm-charts

# Install the operator
helm install odoo-operator bemade/odoo-operator \
  --namespace odoo-operator \
  --create-namespace \
  --set database.host=postgres.default.svc \
  --set database.adminPasswordSecret.name=postgres-admin
```

### Create Your First Odoo Instance

```yaml
apiVersion: bemade.org/v1
kind: OdooInstance
metadata:
  name: my-odoo
  namespace: default
spec:
  image: odoo:18.0
  replicas: 1
  adminPassword: "my-admin-password"
  ingress:
    hosts:
      - my-odoo.example.com
    issuer: letsencrypt-prod
  resources:
    requests:
      cpu: 200m
      memory: 512Mi
    limits:
      cpu: 2000m
      memory: 2Gi
  filestore:
    storageSize: 5Gi
    storageClass: standard
```

Apply with:
```bash
kubectl apply -f my-odoo.yaml
```

## Custom Resource Definitions

### OdooInstance

The main resource for defining an Odoo deployment.

#### Spec Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `image` | string | Yes | `odoo:18.0` | Docker image for Odoo |
| `imagePullSecret` | string | No | - | Secret for private registry |
| `replicas` | integer | Yes | `1` | Number of Odoo pods |
| `adminPassword` | string | Yes | - | Odoo database admin password |
| `resources.requests.cpu` | string | No | `200m` | CPU request |
| `resources.requests.memory` | string | No | `250Mi` | Memory request |
| `resources.limits.cpu` | string | No | `2000m` | CPU limit |
| `resources.limits.memory` | string | No | `2Gi` | Memory limit |
| `filestore.storageSize` | string | No | `2Gi` | PVC size for filestore |
| `filestore.storageClass` | string | No | `standard` | Storage class for PVC |
| `ingress.hosts` | []string | Yes | - | List of hostnames |
| `ingress.issuer` | string | Yes | - | cert-manager ClusterIssuer name |
| `configOptions` | map | No | - | Custom odoo.conf options |
| `initialization.mode` | string | No | `fresh` | `fresh` or `restore` |
| `initialization.restore.*` | object | No | - | Restore config (see below) |
| `upgrade.modules` | []string | No | - | Modules to upgrade |
| `webhook.url` | string | No | - | URL for status notifications |

#### Initialization from Another Odoo Instance

To create an instance by restoring from an existing Odoo:

```yaml
spec:
  initialization:
    mode: restore
    restore:
      url: "https://source-odoo.example.com"
      sourceDatabase: "production"
      masterPassword: "source-master-password"
      withFilestore: true
      neutralize: true  # Reset UUIDs, secrets, disable crons
```

#### Status Fields

| Field | Description |
|-------|-------------|
| `phase` | Current state: `Running`, `Upgrading`, `Restoring` |
| `ready` | Whether the instance is ready to serve traffic |
| `url` | The primary URL of the instance |
| `message` | Human-readable status message |
| `conditions` | Detailed condition array |

### OdooBackupJob

Triggers a backup of an OdooInstance to S3-compatible storage.

```yaml
apiVersion: bemade.org/v1
kind: OdooBackupJob
metadata:
  name: my-odoo-backup
  namespace: default
spec:
  odooInstanceRef:
    name: my-odoo
  format: zip  # zip, sql, or dump
  withFilestore: true
  destination:
    bucket: odoo-backups
    objectKey: my-odoo/2024-01-15/backup.zip
    endpoint: https://minio.example.com
    region: us-east-1
    s3CredentialsSecretRef:
      name: s3-credentials
      namespace: odoo-operator
  webhook:
    url: https://my-app.example.com/backup-complete
    token: "my-webhook-token"
```

#### Backup Formats

| Format | Description | Use Case |
|--------|-------------|----------|
| `zip` | Odoo native format with filestore | Full backup with attachments |
| `dump` | PostgreSQL custom format | Database-only, efficient compression |
| `sql` | Plain SQL text | Human-readable, portable |

### OdooRestoreJob

Restores a backup to an OdooInstance.

#### Restore from S3

```yaml
apiVersion: bemade.org/v1
kind: OdooRestoreJob
metadata:
  name: restore-my-odoo
  namespace: default
spec:
  odooInstanceRef:
    name: my-odoo
  source:
    type: s3
    s3:
      bucket: odoo-backups
      objectKey: my-odoo/2024-01-15/backup.zip
      endpoint: https://minio.example.com
      s3CredentialsSecretRef:
        name: s3-credentials
  format: zip
  neutralize: true
```

#### Restore from Live Odoo Instance

```yaml
apiVersion: bemade.org/v1
kind: OdooRestoreJob
metadata:
  name: clone-production
  namespace: default
spec:
  odooInstanceRef:
    name: staging-odoo
  source:
    type: odoo
    odoo:
      url: "https://production.example.com"
      sourceDatabase: "production"
      masterPassword: "master-password"
  format: zip
  neutralize: true
```

#### The `neutralize` Option

When `neutralize: true` (default), the restore process:
- Resets `database.uuid` and `database.secret`
- Clears `web.base.url`
- Resets login cooldown parameters
- (For zip format) Odoo's `-n` flag also disables crons, mail servers, etc.

Set `neutralize: false` when **moving** an instance (preserving identity):
- UUIDs and secrets are preserved
- Crons and mail servers remain active
- Use for production migrations

## Configuration

### Helm Values

See `values.yaml` for all options. Key settings:

```yaml
# Operator image
operator:
  image: registry.bemade.org/bemade/odoo-operator
  resources:
    requests:
      cpu: 50m
      memory: 64Mi

# Database connection
database:
  host: "postgres"
  port: 5432
  adminUser: "postgres"
  adminPasswordSecret:
    name: "postgres-admin"
    key: "password"

# Default settings for new OdooInstances
defaults:
  odooImage: odoo:18.0
  storageClass: "standard"
```

### S3 Credentials Secret

Create a secret for S3 access:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: s3-credentials
  namespace: odoo-operator
type: Opaque
stringData:
  accessKey: "your-access-key"
  secretKey: "your-secret-key"
```

## Development

### Running Locally

```bash
# Install dependencies
pip install -r requirements.txt

# Set environment variables
export POSTGRES_HOST=localhost
export POSTGRES_ADMIN_USER=postgres
export POSTGRES_ADMIN_PASSWORD=password

# Run the operator
kopf run src/operator.py --verbose
```

### Building the Docker Image

```bash
docker build -t odoo-operator:dev .
```

## License

Proprietary - Bemade Inc.
