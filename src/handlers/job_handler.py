from __future__ import annotations
from .resource_handler import ResourceHandler, create_if_missing
from kubernetes import client
from typing import Optional, TYPE_CHECKING
from typing import cast
import logging
import time

if TYPE_CHECKING:
    from .odoo_handler import OdooHandler

logger = logging.getLogger(__name__)


class JobHandler(ResourceHandler):
    def __init__(
        self,
        handler: OdooHandler,
        status_key: str,
        status_phase: str,
        completion_patch: Optional[dict] = None,
    ):
        super().__init__(handler)
        self.status_key = status_key
        self.status_phase = status_phase
        self.completion_patch = completion_patch

    def _read_resource(self) -> Optional[client.V1Job]:
        # Get the job name from the OdooInstance status, if running
        job_name = self.handler.resource.get("status", {}).get(self.status_key)
        if job_name:
            return cast(
                client.V1Job,
                client.BatchV1Api().read_namespaced_job(
                    name=job_name,
                    namespace=self.namespace,
                ),
            )
        return None

    def handle_create(self):
        if not self._should_run():
            return
        job_spec = self._get_resource_body()
        # Scale down the deployment to avoid conflicts
        self.handler.deployment.scale(0)
        # Create the job
        job = client.BatchV1Api().create_namespaced_job(
            namespace=self.namespace, body=job_spec
        )
        job = cast(client.V1Job, job)
        # Update the OdooInstnace status to show the job is running
        client.CustomObjectsApi().patch_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.handler.namespace,
            plural="odooinstances",
            name=self.handler.name,
            body={
                "status": {
                    "phase": self.status_phase,
                    self.status_key: job.metadata and job.metadata.name,
                },
            },
        )

    @create_if_missing
    def handle_update(self):
        # If the job has completed, scale the deployment up
        if self.resource and self.resource.status:
            status = self.resource.status
            if status.succeeded or status.failed:
                # Update the OdooInstance status to show the job is completed
                self.handler.deployment.scale(1)
                logger.debug(
                    f"{self.status_key} completed. Scaling up deployment {self.handler.deployment.name}"
                )
                client.CustomObjectsApi().patch_namespaced_custom_object_status(
                    group="bemade.org",
                    version="v1",
                    namespace=self.handler.namespace,
                    plural="odooinstances",
                    name=self.handler.name,
                    body={
                        "status": {
                            "phase": "Running",
                            self.status_key: None,
                        },
                    },
                )
                if self.completion_patch:
                    logger.debug(
                        f"{self.status_key} completed. Patching OdooInstance {self.handler.name} with {self.completion_patch}"
                    )
                    client.CustomObjectsApi().patch_namespaced_custom_object(
                        group="bemade.org",
                        version="v1",
                        namespace=self.handler.namespace,
                        plural="odooinstances",
                        name=self.handler.name,
                        body=self.completion_patch,
                    )

    def _get_resource_body(self) -> client.V1Job:
        raise NotImplementedError()

    def _should_run(self):
        return not self.is_running

    @property
    def is_running(self) -> bool:
        max_tries = 3
        tries = 0
        resource = None
        while tries < max_tries and not resource:
            try:
                resource = self.resource
            except client.ApiException as e:
                if e.status == 404:
                    time.sleep(1)
                else:
                    raise
            tries += 1
        if not resource:
            return False
        return not (resource.status.succeeded or resource.status.failed)

        # Because the job itself is sometimes not created even though it's been requested
        # due to kubernetes coordination latency, we check the OdooInstance status instead.

        # status = client.CustomObjectsApi().get_namespaced_custom_object_status(
        #     group="bemade.org",
        #     version="v1",
        #     namespace=self.handler.namespace,
        #     plural="odooinstances",
        #     name=self.handler.name,
        # )
        # if not status:
        #     raise Exception("OdooInstance status could not be loaded.")
        # status = cast(dict, status).get("status", {})
        # running = status.get("phase") == self.status_phase
        # if self.resource:
        #     logger.debug(
        #         f"Job {self.resource.metadata.name} {"is" if running else "is not"} running. OdooInstance in phase: {status.get("phase")}"
        #     )
        # return running
