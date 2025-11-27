from kubernetes import client
from kubernetes.client.rest import ApiException
import kopf
import os
import yaml
import requests
from typing import cast

from .pull_secret import PullSecret
from .odoo_user_secret import OdooUserSecret
from .filestore_pvc import FilestorePVC
from .odoo_conf import OdooConf
from .tls_cert import TLSCert
from .deployment import Deployment
from .service import Service
from .ingress_routes import IngressRouteHTTPS, IngressRouteWebsocket
from .upgrade_job import UpgradeJob
from .restore_job import RestoreJob
from .resource_handler import ResourceHandler
import logging
from enum import Enum


class Stage(Enum):
    RUNNING = "Running"
    UPGRADING = "Upgrading"


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
        self.ingress_route_https = IngressRouteHTTPS(self)
        self.ingress_route_websocket = IngressRouteWebsocket(self)
        self.upgrade_job = UpgradeJob(self)
        self.restore_job = RestoreJob(self)

        # Create handlers list in the correct order for creation/update
        self.handlers = [
            self.pull_secret,
            self.odoo_user_secret,
            self.filestore_pvc,
            self.odoo_conf,
            self.tls_cert,
            self.deployment,
            self.service,
            self.ingress_route_https,
            self.ingress_route_websocket,
        ]

        # The Job handlers (upgrade, restore) are handled separately

    def on_create(self):
        logging.info(f"Creating OdooInstance {self.name}")
        # Create all resources in the correct order
        for handler in self.handlers:
            handler.handle_create()

        # Initialize status to Running - database initialization is handled externally
        # (either Odoo auto-initializes on first start, or a RestoreJob is created)
        self._initialize_status()

    def on_update(self):
        """Handle update events for this OdooInstance."""
        logging.info(f"Handling update for OdooInstance {self.name}")

        if self._is_restore_request() and self._should_execute_restore():
            logging.info(f"Restore requested for {self.name}")
            self._handle_restore()
        # Check if this is an upgrade request that should be executed now
        if self._is_upgrade_request() and self._should_execute_upgrade():
            logging.info(f"Upgrade requested for {self.name}")
            self._handle_upgrade()

        # Always update resource handlers
        logging.debug(f"Running regular update for handlers of {self.name}")
        for handler in self.handlers:
            handler.handle_update()

    def on_delete(self):
        # Delete resources in reverse order
        # The deployment handler will handle scaling down before deletion
        for handler in reversed(self.handlers):
            handler.handle_delete()

    def _is_restore_request(self):
        """Check if the spec contains a valid restore request."""
        restore_spec = self.spec.get("restore", {})
        return restore_spec and restore_spec.get("enabled", False)

    def _should_execute_restore(self):
        """Determine if a restore request should be executed now.
        Check if:

        1. If a restore job is already running
        2. If a restore job is requested
        """
        return not self.restore_job.is_running and self._is_restore_request()

    def _handle_restore(self):
        """Handle the restore process."""
        logging.info(f"Starting restore process for {self.name}")

        # Create or update the restore job
        self.restore_job.handle_update()

        # The job will run asynchronously, and we'll check for completion
        # in the check_restore_job_completion method that will be called periodically
        logging.debug(
            f"Restore job created for {self.name}, will check for completion periodically"
        )

    def _is_upgrade_request(self):
        """Check if the spec contains a valid upgrade request."""
        upgrade_spec = self.spec.get("upgrade", {})
        modules = upgrade_spec.get("modules", [])

        # Basic validation that this is an upgrade request
        # Database name is now auto-generated from UID, so we don't need to check for it
        return upgrade_spec and isinstance(modules, list) and len(modules) > 0

    def _should_execute_upgrade(self):
        """
        Determine if an upgrade request should be executed now.

        This checks:
        1. If a restore job is already running
        2. If an upgrade job is already running
        3. If a scheduled time is specified and has passed
        """
        if self.restore_job.is_running:
            return False
        if self.upgrade_job.is_running:
            return False

        # If it's not a valid upgrade request, don't execute
        if not self._is_upgrade_request():
            return False

        upgrade_spec = self.spec.get("upgrade", {})
        scheduled_time = upgrade_spec.get("time", "")

        # If no scheduled time is specified, execute immediately
        if not scheduled_time:
            return True

        # If a scheduled time is specified, check if it's time to execute
        try:
            # Parse the scheduled time
            from datetime import datetime, timezone

            # Parse the ISO format time string
            scheduled_datetime = datetime.fromisoformat(
                scheduled_time.replace("Z", "+00:00")
            )

            # Get the current time in UTC
            current_time = datetime.now(timezone.utc)

            # If the scheduled time has passed, it's time to execute the upgrade
            return current_time >= scheduled_datetime
        except Exception as e:
            logging.error(
                f"Error parsing scheduled upgrade time for {self.name}: {e}",
                exc_info=True,
            )
            # If there's an error parsing the time, default to not upgrading
            return False

    def check_periodic(self):
        """
        Periodic checks for this instance.
        """
        # Check if the status is somehow out of date

        self._check_status()

        # Check for scheduled upgrades
        self._check_scheduled_upgrade()

        # Add any future periodic checks here
        # ...

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
                        "initJob": None,
                        "restoreJob": None,
                        "upgradeJob": None,
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

    def _check_status(self):
        """
        Check if the status is somehow out of date.
        """
        logging.debug(f"Checking status for {self.name}")
        status = client.CustomObjectsApi().get_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=self.name,
        )
        phase = cast(dict, status).get("status", {}).get("phase")
        if not phase:
            logging.info(f"Status for {self.name} is missing. Initializing.")
            self._initialize_status()
            return
        if phase == "Running":
            logging.debug(f"Status for {self.name} is Running. No action needed.")
            return
        if phase == "Upgrading" and self.upgrade_job.is_running:
            logging.debug(f"Status for {self.name} is Upgrading. No action needed.")
            return
        if phase == "Restoring" and self.restore_job.is_running:
            logging.debug(f"Status for {self.name} is Restoring. No action needed.")
            return
        logging.debug(
            f"Resetting status to Running since job for {phase} is not running."
        )
        client.CustomObjectsApi().patch_namespaced_custom_object_status(
            group="bemade.org",
            version="v1",
            namespace=self.namespace,
            plural="odooinstances",
            name=self.name,
            body={
                "status": {
                    "phase": "Running",
                    "initJob": None,
                    "restoreJob": None,
                    "upgradeJob": None,
                }
            },
        )

    def _check_scheduled_upgrade(self):
        """
        Check if this instance has a scheduled upgrade that should be executed now.
        """
        logging.debug(f"Checking for scheduled upgrades for {self.name}")

        # Check if this is an upgrade request that should be executed now
        if self._is_upgrade_request() and self._should_execute_upgrade():
            logging.info(f"Executing scheduled upgrade for {self.name}")
            self._handle_upgrade()

    def _handle_upgrade(self):
        """Handle the upgrade process."""
        logging.info(f"Starting upgrade process for {self.name}")

        # Create or update the upgrade job
        self.upgrade_job.handle_update()

        # The job will run asynchronously, and we'll check for completion
        # in the check_upgrade_job_completion method that will be called periodically
        logging.debug(
            f"Upgrade job created for {self.name}, will check for completion periodically"
        )

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

            # Get database connection parameters from environment variables
            db_host = os.environ.get("DB_HOST")
            db_port = os.environ.get("DB_PORT")
            db_superuser = os.environ.get("DB_ADMIN_USER")
            db_superuser_password = os.environ.get("DB_ADMIN_PASSWORD")

            # Get the Odoo username for ownership check
            odoo_username = self.odoo_user_secret.username
            if not odoo_username:
                return False, "Could not retrieve Odoo database username"

            # Connect to the postgres database using superuser credentials
            conn = psycopg2.connect(
                host=db_host,
                port=db_port,
                database="postgres",
                user=db_superuser,
                password=db_superuser_password,
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

        match body.get("metadata", {}).get("name"):
            case name if "upgrade" in name:
                self.upgrade_job.handle_update()
            case name if "restore" in name:
                self.restore_job.handle_update()
