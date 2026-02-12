# odoo-operator

A Kubernetes operator for managing Odoo instances. Declaratively deploy, initialize,
upgrade, back up, and restore Odoo databases using custom resources.

Built with [kubebuilder](https://book.kubebuilder.io/) / controller-runtime. Deployed
via Helm.

## Custom Resources

| Resource | Purpose |
|---|---|
| `OdooInstance` | Declares a running Odoo deployment: image, replicas, ingress, filestore, database |
| `OdooInitJob` | One-shot job to initialize a fresh database |
| `OdooUpgradeJob` | Runs `odoo -u all` against an existing database, then rolls the deployment |
| `OdooBackupJob` | Dumps the database and filestore to object storage |
| `OdooRestoreJob` | Restores a backup into an OdooInstance |

## Prerequisites

- Kubernetes 1.26+
- cert-manager (for webhook TLS)
- A PostgreSQL cluster accessible from the operator namespace
- Helm 3

## Installation

### 1. Create the postgres clusters secret

```yaml
# clusters.yaml
main:
  host: postgres.postgres.svc.cluster.local
  port: 5432
  adminUser: postgres
  adminPassword: secret
  default: true
```

```sh
kubectl create namespace odoo-operator
kubectl create secret generic pg-clusters -n odoo-operator \
  --from-file=clusters.yaml=clusters.yaml
```

### 2. Install the chart

#### With a values file (recommended)

Get the sample values file at:

`https://raw.githubusercontent.com/bemade/odoo-operator/refs/heads/main/kubebuilder/charts/odoo-operator/values.yaml`

Edit the file according to your requirements, then:

```sh
helm upgrade --install odoo-operator oci://registry.bemade.org/charts/odoo-operator \
  --namespace odoo-operator \
  -f <values_file_path.yaml>
```

If installing with Rancher or similar tools, simply choose to edit the helm values
prior to installation.

#### With command line values

```sh
helm upgrade --install odoo-operator oci://registry.bemade.org/charts/odoo-operator \
  --namespace odoo-operator \
  --set postgresClustersSecretRef.name=pg-clusters \
  --set defaults.ingressClass=nginx \
  --set defaults.ingressIssuer=letsencrypt-prod
```

### 3. Deploy an Odoo instance

```yaml
apiVersion: bemade.org/v1alpha1
kind: OdooInstance
metadata:
  name: myodoo
  namespace: odoo
spec:
  image: odoo:18.0
  adminPassword: changeme
  replicas: 1
  ingress:
    hosts:
      - myodoo.example.com
```

```sh
kubectl apply -f odoo.yaml

# Initialize the database
kubectl apply -f - <<EOF
apiVersion: bemade.org/v1alpha1
kind: OdooInitJob
metadata:
  name: myodoo-init
  namespace: odoo
spec:
  odooInstanceRef:
    name: myodoo
EOF
```

## Configuration Reference

### OdooInstance spec

| Field | Default | Description |
|---|---|---|
| `image` | operator default | Odoo container image |
| `replicas` | `1` | Number of Odoo pods. Set to `0` to stop |
| `adminPassword` | — | Odoo master password |
| `ingress.hosts` | — | Hostnames to expose the instance on |
| `ingress.issuer` | operator default | cert-manager ClusterIssuer for TLS |
| `ingress.class` | operator default | IngressClass name |
| `database.cluster` | secret default | Postgres cluster name from the pg-clusters secret |
| `filestore.storageSize` | `2Gi` | PVC size. Can only be increased, not decreased |
| `filestore.storageClass` | operator default | StorageClass for the filestore PVC. Immutable after creation |
| `resources` | operator default | CPU/memory requests and limits |
| `strategy.type` | `Recreate` | Deployment strategy (`Recreate` or `RollingUpdate`) |
| `probes.*` | `/web/health` | Liveness/readiness/startup probe paths |
| `configOptions` | — | Extra key-value pairs appended to `odoo.conf` |
| `affinity` | operator default | Pod affinity rules |
| `tolerations` | operator default | Pod tolerations |

### Webhook validation

The validating webhook rejects:
- `spec.filestore.storageSize` decreases
- `spec.filestore.storageClass` changes after initial set
- `spec.database.cluster` changes after initial set (cluster migration not yet implemented)

### Operator flags

| Flag | Default | Description |
|---|---|---|
| `--postgres-clusters-secret` | `postgres-clusters` | Secret name in operator namespace |
| `--default-odoo-image` | `odoo:18.0` | Image used when `spec.image` is unset |
| `--default-storage-class` | `standard` | StorageClass when `spec.filestore.storageClass` is unset |
| `--default-storage-size` | `2Gi` | PVC size when `spec.filestore.storageSize` is unset |
| `--default-ingress-class` | — | IngressClass when `spec.ingress.class` is unset |
| `--default-ingress-issuer` | — | ClusterIssuer when `spec.ingress.issuer` is unset |
| `--default-resources` | — | JSON `ResourceRequirements` when `spec.resources` is unset |
| `--default-affinity` | — | JSON `Affinity` when `spec.affinity` is unset |
| `--default-tolerations` | — | JSON `[]Toleration` when `spec.tolerations` is unset |

## Development

```sh
# Run tests
cd operator && go test ./...

# Build and load into minikube
~/reload-operator.sh

# Regenerate CRD manifests after type changes
cd operator && make manifests && make helm-crds
```

## License

LGPL-3.0-or-later — Copyright 2026 Marc Durepos, Bemade Inc.
