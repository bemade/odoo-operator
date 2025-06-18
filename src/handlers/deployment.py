from __future__ import annotations
from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
import logging
import os
from typing import cast, TYPE_CHECKING

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

        instance_status = client.CustomObjectsApi().get_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.handler.namespace,
            plural="odooinstances",
            name=self.handler.name,
        )
        if not instance_status:
            raise Exception("OdooInstance status could not be loaded.")
        phase = cast(dict, instance_status).get("status", {}).get("phase")

        logger.debug(f"Deployment {self.name} checking OdooInstance phase: {phase}")

        if phase in ["Syncing", "Upgrading"]:
            deployment.spec.replicas = 0
            logger.debug(f"Deployment {self.name} scaled down to 0 replicas")

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

    def scale(self, replicas):
        """Scale the deployment to the specified number of replicas."""
        try:
            # Get the current deployment
            deployment = client.AppsV1Api().read_namespaced_deployment(
                name=self.name,
                namespace=self.namespace,
            )
            deployment = cast(client.V1Deployment, deployment)

            if not deployment.spec:
                raise Exception(f"Deployment {self.name} not found, couldn't scale.")
            # Update the replicas
            deployment.spec.replicas = replicas

            # Apply the update
            client.AppsV1Api().patch_namespaced_deployment(
                name=self.name,
                namespace=self.namespace,
                body={"spec": {"replicas": replicas}},
            )

            return True
        except client.ApiException as e:
            if e.status != 404:
                raise
            return False

    def _get_resource_body(self) -> client.V1Deployment:
        db_host = os.environ["DB_HOST"]
        db_port = os.environ["DB_PORT"]

        image = self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0"))

        # Define volumes
        volumes = [
            client.V1Volume(
                name="filestore",
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=f"{self.name}-filestore-pvc"
                ),
            ),
            client.V1Volume(
                name="odoo-conf",
                config_map=client.V1ConfigMapVolumeSource(
                    name=f"{self.name}-odoo-conf"
                ),
            ),
        ]

        # Add Git repository volume if configured
        if self.spec.get("gitProject"):
            volumes += [
                client.V1Volume(
                    name="repo-volume",
                    persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                        claim_name=f"{self.name}-repo-pvc"
                    ),
                ),
                client.V1Volume(
                    name="python-deps",
                    empty_dir=client.V1EmptyDirVolumeSource(),
                ),
            ]

        # Define volume mounts
        volume_mounts = [
            client.V1VolumeMount(
                name="filestore",
                mount_path="/var/lib/odoo",
            ),
            client.V1VolumeMount(
                name="odoo-conf",
                mount_path="/etc/odoo",
            ),
            client.V1VolumeMount(
                name="python-deps",
                mount_path="/mnt/python-deps",
            ),
        ]

        # Add Git repository volume mount if configured
        if self.spec.get("gitProject"):
            # Mount the entire git repo to /mnt/repo
            volume_mounts.append(
                client.V1VolumeMount(name="repo-volume", mount_path="/mnt/repo")
            )

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

        # Get replicas from spec or default to 1
        replicas = self.spec.get("replicas", 1)
        python_path_var = client.V1EnvVar(
            name="PYTHONPATH",
            value="/mnt/python-deps",
        )

        spec = client.V1DeploymentSpec(
            replicas=replicas,
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
                    init_containers=[
                        self._get_init_container_spec(volume_mounts, python_path_var)
                    ],
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
                            env=[
                                client.V1EnvVar(
                                    name="HOST",
                                    value=db_host,
                                ),
                                client.V1EnvVar(
                                    name="PORT",
                                    value=db_port,
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
                                python_path_var,
                            ],
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
                                period_seconds=2,
                                timeout_seconds=2,
                                success_threshold=1,
                                failure_threshold=36,
                            ),
                            readiness_probe=client.V1Probe(
                                http_get=client.V1HTTPGetAction(
                                    path="/web/health",
                                    port=8069,
                                ),
                                initial_delay_seconds=2,
                                period_seconds=2,
                                timeout_seconds=2,
                                success_threshold=1,
                                failure_threshold=20,
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

    def _get_init_container_spec(
        self,
        volumeMounts: [client.V1VolumeMount],
        python_path_var: client.V1EnvVar,
    ):
        python_path = python_path_var.value
        return client.V1Container(
            name="pip-install",
            image=self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0")),
            image_pull_policy="IfNotPresent",
            command=["/bin/bash", "-c"],
            args=[
                """
                # Copy system packages into persisted volume
                cp -r /usr/lib/python3/dist-packages/* "%(python_path)s/"

                # Check for requirements.txt and install if present
                REQUIREMENTS_FILE="/mnt/repo/odoo-code/requirements.txt"
                if [ -f "$REQUIREMENTS_FILE" ]; then
                    echo "Found requirements.txt, installing Python dependencies..."

                    # Check pip version to determine if we need --break-system-packages
                    MAJOR_VERSION=$(pip --version | awk '{print $2}' | cut -d. -f1)

                    # Install requirements
                    set -e  # Exit immediately if a command fails
                    if [ "$MAJOR_VERSION" -ge 23 ]; then
                        echo "Using pip $MAJOR_VERSION.x with --break-system-packages"
                        pip install --break-system-packages -r "$REQUIREMENTS_FILE" --target "%(python_path)s"
                    else
                        echo "Using pip $MAJOR_VERSION.x"
                        pip install -r "$REQUIREMENTS_FILE" --target "%(python_path)s"
                    fi

                    echo "Python requirements installed successfully"
                else
                    echo "No requirements.txt found, skipping Python dependencies installation"
                fi
            """
                % {"python_path": python_path}
            ],
            volume_mounts=volumeMounts,
            security_context=client.V1SecurityContext(
                privileged=True,
                run_as_user=0,
                run_as_group=0,
            ),
            env=[python_path_var],
        )
