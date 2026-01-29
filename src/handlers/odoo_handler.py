from kubernetes import client
import os
import yaml
import requests
from typing import cast

from .postgres_clusters import get_cluster_for_instance
from .pull_secret import PullSecret
from .odoo_user_secret import OdooUserSecret
from .filestore_pvc import FilestorePVC
from .odoo_conf import OdooConf
from .tls_cert import TLSCert
from .deployment import Deployment
from .service import Service
from .ingress import Ingress
from .resource_handler import ResourceHandler
import logging
from enum import Enum


class Stage(Enum):
    RUNNING = "Running"


class OdooHandler(ResourceHandler):
    def __init__(self, body=None, **kwargs):
        if body:
            self.body = body
            self.spec = body.get("spec", {})
            self.meta = body.get("meta", body.get("metadata"))
            self.namespace = self.meta.get("namespace")
            self.name = self.meta.get("name")
            self.uid = self.meta.get("uid")
        else:
            self.body = {}
            self.spec = {}
            self.meta = {}
            self.namespace = None
            self.name = None
            self.uid = None

        self.operator_ns = os.environ.get("OPERATOR_NAMESPACE")

        # Load defaults if available
        try:
            with open("/etc/odoo/instance-defaults.yaml") as f:
                self.defaults = yaml.safe_load(f)
        except (FileNotFoundError, PermissionError):
            self.defaults = {}

        self._resource = None  # This will be an OdooInstance

        # Initialize all handlers in the correct order for creation/update
        # Each handler will check the spec to determine if it should create resources
        self.pull_secret = PullSecret(self)
        self.odoo_user_secret = OdooUserSecret(self)
        self.filestore_pvc = FilestorePVC(self)
        self.odoo_conf = OdooConf(self)
        self.tls_cert = TLSCert(self)
        self.deployment = Deployment(self)
        self.service = Service(self)
        self.ingress = Ingress(self)

        # Create handlers list in the correct order for creation/update
        self.handlers = [
            self.pull_secret,
            self.odoo_user_secret,
            self.filestore_pvc,
            self.odoo_conf,
            self.tls_cert,
            self.deployment,
            self.service,
            self.ingress,
        ]

    def on_create(self):
        logging.info(f"Creating OdooInstance {self.name}")
        # Create all resources in the correct order
        for handler in self.handlers:
            handler.handle_create()

        # Initialize status to Running - database initialization is handled externally
        # (either Odoo auto-initializes on first start, or an OdooRestoreJob CR is created)
        self._initialize_status()

    def on_update(self):
        """Handle update events for this OdooInstance."""
        logging.info(f"Handling update for OdooInstance {self.name}")
        # NOTE: Restores handled via OdooRestoreJob CRD, upgrades via OdooUpgradeJob CRD

        # Update resource handlers
        logging.debug(f"Running regular update for handlers of {self.name}")
        for handler in self.handlers:
            handler.handle_update()

    def on_delete(self):
        # Delete resources in reverse order
        # The deployment handler will handle scaling down before deletion
        for handler in reversed(self.handlers):
            handler.handle_delete()

    def _initialize_status(self):
        """
        Initialize the status for a newly created OdooInstance.
        """
        logging.info(f"Initializing status for {self.name}")
        try:
            client.CustomObjectsApi().patch_namespaced_custom_object_status(
                group="bemade.org",
                version="v1",
                namespace=self.namespace,
                plural="odooinstances",
                name=self.name,
                body={
                    "status": {
                        "phase": "Running",
                    }
                },
            )
            logging.info(f"Status initialized for {self.name}")
            # Notify via webhook if configured
            self._call_webhook("Running")
        except Exception as e:
            logging.error(f"Failed to initialize status for {self.name}: {e}")

    def _call_webhook(self, phase: str, message: str = ""):
        """Call the webhook URL if configured in the spec."""
        webhook = self.spec.get("webhook", {})
        url = webhook.get("url")
        if not url:
            return

        try:
            payload = {
                "phase": phase,
                "message": message,
            }
            response = requests.post(
                url,
                json=payload,
                timeout=10,
                verify=True,
            )
            if response.status_code == 200:
                logging.info(f"Webhook called successfully for {self.name}: {phase}")
            else:
                logging.warning(
                    f"Webhook returned {response.status_code} for {self.name}: {response.text}"
                )
        except Exception as e:
            logging.warning(f"Failed to call webhook for {self.name}: {e}")

    def validate_database_exists(self, database_name):
        """
        Validate that the specified database exists and belongs to the Odoo user.

        Args:
            database_name: The name of the database to validate

        Returns:
            tuple: (exists, error_message) where exists is a boolean and error_message is None if exists is True
        """
        try:
            import psycopg2

            # Get the PostgreSQL cluster configuration for this instance
            pg_cluster = get_cluster_for_instance(self.spec)

            # Get the Odoo username for ownership check
            odoo_username = self.odoo_user_secret.username
            if not odoo_username:
                return False, "Could not retrieve Odoo database username"

            # Connect to the postgres database using superuser credentials
            conn = psycopg2.connect(
                host=pg_cluster.host,
                port=pg_cluster.port,
                database="postgres",
                user=pg_cluster.admin_user,
                password=pg_cluster.admin_password,
            )

            # Set autocommit to True to avoid transaction issues
            conn.autocommit = True

            # Create a cursor
            cur = conn.cursor()

            # Check if the database exists
            cur.execute(
                "SELECT datname FROM pg_database WHERE datname = %s", (database_name,)
            )

            database_exists = cur.fetchone() is not None

            if not database_exists:
                cur.close()
                conn.close()
                return False, f"Database '{database_name}' does not exist"

            # Check if the database is owned by the Odoo user
            cur.execute(
                """
                SELECT d.datname
                FROM pg_database d
                JOIN pg_roles r ON d.datdba = r.oid
                WHERE d.datname = %s AND r.rolname = %s
                """,
                (database_name, odoo_username),
            )

            owned_by_odoo_user = cur.fetchone() is not None

            # Close cursor and connection
            cur.close()
            conn.close()

            if not owned_by_odoo_user:
                return (
                    False,
                    f"Database '{database_name}' is not owned by the Odoo user",
                )

            return True, None

        except Exception as e:
            logging.error(
                f"Error validating database existence for {database_name}: {e}"
            )
            return False, f"Error validating database: {str(e)}"

    @classmethod
    def from_job_info(cls, namespace, app_name):
        """Create an OdooHandler instance from job information.

        Args:
            namespace: The namespace of the job
            app_name: The name of the OdooInstance

        Returns:
            An OdooHandler instance or None if the OdooInstance doesn't exist
        """
        try:
            # Get the OdooInstance resource
            api = client.CustomObjectsApi()
            try:
                odoo_instance = api.get_namespaced_custom_object(
                    group="bemade.org",
                    version="v1",
                    namespace=namespace,
                    plural="odooinstances",
                    name=app_name,
                )

                # Create and return a handler with the OdooInstance as the body
                # The CustomObjectsApi returns the resource as a dictionary,
                # which is exactly what the constructor expects
                return cls(odoo_instance)

            except client.ApiException as e:
                if e.status == 404:
                    logging.warning(
                        f"OdooInstance {app_name} not found, it may have been deleted"
                    )
                    return None
                else:
                    raise
        except Exception as e:
            logging.error(f"Error creating OdooHandler from job info: {e}")
            return None

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="bemade.org/v1",
            kind="OdooInstance",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    def _read_resource(self):
        api = client.CustomObjectsApi()
        return api.get_namespaced_custom_object(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=self.name,
        )

    @property
    def status(self) -> dict:
        full_status = client.CustomObjectsApi().get_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=self.name,
        )
        if not full_status:
            raise Exception("OdooInstance status could not be loaded.")
        return cast(dict, full_status).get("status", {})

    @property
    def stage(self) -> Stage:
        return Stage(self.status.get("phase", "Running"))

    def handle_job_completion(self, body: dict):
        """Handle completion of various types of jobs for this instance."""
        # NOTE: Restore/upgrade jobs now handled via separate CRDs
        pass
