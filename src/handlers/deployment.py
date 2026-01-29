from __future__ import annotations
from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
from .postgres_clusters import get_cluster_for_instance
import logging
from typing import TYPE_CHECKING, Tuple, List

if TYPE_CHECKING:
    from .odoo_handler import OdooHandler

logger = logging.getLogger(__name__)


class Deployment(ResourceHandler):
    """Manages the Odoo Deployment."""

    def __init__(self, handler: OdooHandler):
        super().__init__(handler)
        self.defaults = handler.defaults
        self.odoo_user_secret = handler.odoo_user_secret

    def _read_resource(self):
        return client.AppsV1Api().read_namespaced_deployment(
            name=self.name,
            namespace=self.namespace,
        )

    def _get_desired_replicas(self) -> int:
        """Get the desired replica count, preserving current value if deployment exists."""
        if self.resource and self.resource.spec:
            return self.resource.spec.replicas or 0
        return 0

    @update_if_exists
    def handle_create(self):
        deployment = self._get_resource_body()
        self._resource = client.AppsV1Api().create_namespaced_deployment(
            namespace=self.namespace,
            body=deployment,
        )

    @create_if_missing
    def handle_update(self):
        deployment = self._get_resource_body()

        if not deployment.spec:
            raise Exception("Deployment spec is missing")

        self._resource = client.AppsV1Api().patch_namespaced_deployment(
            name=self.name,
            namespace=self.namespace,
            body=deployment,
        )

    def handle_delete(self):
        # Scale down the deployment before deletion
        if self.resource:
            self.resource.spec.replicas = 0
            client.AppsV1Api().patch_namespaced_deployment(
                name=self.name,
                namespace=self.namespace,
                body=self.resource,
            )

    def _get_resource_body(self) -> client.V1Deployment:
        image = self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0"))

        volumes, volume_mounts = self.get_volumes_and_mounts()

        metadata = client.V1ObjectMeta(
            name=self.name,
            owner_references=[self.owner_reference],
            labels={"app": self.name},
        )

        pull_secret = (
            {
                "image_pull_secrets": [
                    client.V1LocalObjectReference(
                        name=f"{self.spec.get('imagePullSecret')}"
                    )
                ]
            }
            if self.spec.get("imagePullSecret")
            else {}
        )

        spec = client.V1DeploymentSpec(
            replicas=self._get_desired_replicas(),
            selector=client.V1LabelSelector(match_labels={"app": self.name}),
            strategy={"type": "Recreate"},
            template=client.V1PodTemplateSpec(
                metadata=client.V1ObjectMeta(
                    labels={"app": self.name},
                ),
                spec=client.V1PodSpec(
                    **pull_secret,
                    volumes=volumes,
                    security_context=client.V1PodSecurityContext(
                        run_as_user=100,
                        run_as_group=101,
                        fs_group=101,
                    ),
                    affinity=self.spec.get(
                        "affinity", self.defaults.get("affinity", {})
                    ),
                    tolerations=self.spec.get(
                        "tolerations", self.defaults.get("tolerations", [])
                    ),
                    containers=[
                        client.V1Container(
                            name=f"odoo-{self.name}",
                            image=image,
                            image_pull_policy="IfNotPresent",
                            command=["/entrypoint.sh", "odoo"],
                            ports=[
                                client.V1ContainerPort(
                                    container_port=8069,
                                    name="http",
                                ),
                                client.V1ContainerPort(
                                    container_port=8072,
                                    name="websocket",
                                ),
                            ],
                            volume_mounts=volume_mounts,
                            env=self.get_environment_variables(),
                            resources=self.spec.get(
                                "resources",
                                self.defaults.get("resources", {}),
                            ),
                            liveness_probe=client.V1Probe(
                                http_get=client.V1HTTPGetAction(
                                    path="/web/health",
                                    port=8069,
                                ),
                                initial_delay_seconds=2,
                                period_seconds=15,
                                timeout_seconds=2,
                                success_threshold=1,
                                failure_threshold=40,
                            ),
                            readiness_probe=client.V1Probe(
                                http_get=client.V1HTTPGetAction(
                                    path="/web/health",
                                    port=8069,
                                ),
                                initial_delay_seconds=2,
                                period_seconds=15,
                                timeout_seconds=2,
                                success_threshold=1,
                                failure_threshold=40,
                            ),
                        )
                    ],
                ),
            ),
        )

        return client.V1Deployment(
            metadata=metadata,
            spec=spec,
        )

    def get_environment_variables(self) -> List[client.V1EnvVar]:
        """Get the environment variables for Odoo pods.

        Returns common environment variables including Python path if configured.
        """
        # Get the PostgreSQL cluster configuration for this instance
        pg_cluster = get_cluster_for_instance(self.spec)

        env_vars = [
            client.V1EnvVar(
                name="HOST",
                value=pg_cluster.host,
            ),
            client.V1EnvVar(
                name="PORT",
                value=str(pg_cluster.port),
            ),
            client.V1EnvVar(
                name="USER",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{self.name}-odoo-user",
                        key="username",
                    )
                ),
            ),
            client.V1EnvVar(
                name="PASSWORD",
                value_from=client.V1EnvVarSource(
                    secret_key_ref=client.V1SecretKeySelector(
                        name=f"{self.name}-odoo-user",
                        key="password",
                    )
                ),
            ),
        ]

        return env_vars

    def get_volumes_and_mounts(
        self,
    ) -> Tuple[List[client.V1Volume], List[client.V1VolumeMount]]:
        return get_odoo_volumes_and_mounts(self.name)


def get_odoo_volumes_and_mounts(
    instance_name: str,
) -> Tuple[List[client.V1Volume], List[client.V1VolumeMount]]:
    """
    Get the standard volumes and volume mounts for an Odoo instance.

    This includes:
    - filestore PVC: persistent storage for attachments and session data
    - odoo-conf ConfigMap: Odoo configuration including addons paths

    Args:
        instance_name: The name of the OdooInstance

    Returns:
        Tuple of (volumes, volume_mounts) for use in pod specs
    """
    volumes = [
        client.V1Volume(
            name="filestore",
            persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                claim_name=f"{instance_name}-filestore-pvc"
            ),
        ),
        client.V1Volume(
            name="odoo-conf",
            config_map=client.V1ConfigMapVolumeSource(
                name=f"{instance_name}-odoo-conf"
            ),
        ),
    ]

    volume_mounts = [
        client.V1VolumeMount(
            name="filestore",
            mount_path="/var/lib/odoo",
        ),
        client.V1VolumeMount(
            name="odoo-conf",
            mount_path="/etc/odoo",
        ),
    ]

    return volumes, volume_mounts
