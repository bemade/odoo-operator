from kubernetes import client
import os
import yaml

from .pull_secret import PullSecret
from .odoo_user_secret import OdooUserSecret
from .filestore_pvc import FilestorePVC
from .odoo_conf import OdooConf
from .tls_cert import TLSCert
from .deployment import Deployment
from .service import Service
from .ingress_routes import IngressRouteHTTP, IngressRouteHTTPS, IngressRouteWebsocket
from .upgrade_job import UpgradeJob
from .resource_handler import ResourceHandler
import logging
import base64


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
        self.ingress_route_http = IngressRouteHTTP(self)
        self.ingress_route_https = IngressRouteHTTPS(self)
        self.ingress_route_websocket = IngressRouteWebsocket(self)
        self.upgrade_job = UpgradeJob(self)

        # Create handlers list in the order resources should be created/updated
        self.handlers = [
            self.pull_secret,
            self.odoo_user_secret,
            self.filestore_pvc,
            self.odoo_conf,
            self.tls_cert,
            self.deployment,
            self.service,
            self.ingress_route_http,
            self.ingress_route_https,
            self.ingress_route_websocket,
        ]

        # The upgrade job is handled separately and not included in the main handlers list

    def on_create(self):
        # Create all resources in the correct order
        for handler in self.handlers:
            handler.handle_create()

    def on_update(self):
        # Check if this is an upgrade request
        if self._is_upgrade_request() and self._should_execute_upgrade():
            self._handle_upgrade()
        else:
            # Regular update - update all resources in the correct order
            for handler in self.handlers:
                handler.handle_update()

    def on_delete(self):
        # Delete resources in reverse order
        # The deployment handler will handle scaling down before deletion
        for handler in reversed(self.handlers):
            handler.handle_delete()

    def _is_upgrade_request(self):
        """Check if the spec contains a valid upgrade request."""
        upgrade_spec = self.spec.get("upgrade", {})
        database = upgrade_spec.get("database", "")
        modules = upgrade_spec.get("modules", [])

        # Basic validation that this is an upgrade request
        return upgrade_spec and database and isinstance(modules, list) and len(modules) > 0

    def _should_execute_upgrade(self):
        """Determine if an upgrade request should be executed now.
        
        This checks:
        1. If an upgrade job is already running
        2. If a scheduled time is specified and has passed
        """
        # If it's not a valid upgrade request, don't execute
        if not self._is_upgrade_request():
            return False
            
        upgrade_spec = self.spec.get("upgrade", {})
        scheduled_time = upgrade_spec.get("time", "")
            
        # If an upgrade job exists and is not completed, don't trigger a new one
        if self.upgrade_job.resource and not self.upgrade_job.is_completed:
            logging.debug(f"Upgrade job for {self.name} is already running, skipping new upgrade request")
            return False

        # If no scheduled time is specified, execute immediately
        if not scheduled_time:
            return True

        # If a scheduled time is specified, check if it's time to execute
        try:
            # Parse the scheduled time
            from datetime import datetime
            import pytz

            # Parse the ISO format time string
            scheduled_datetime = datetime.fromisoformat(
                scheduled_time.replace("Z", "+00:00")
            )

            # Get the current time in UTC
            current_time = datetime.now(pytz.UTC)

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
        Perform all periodic checks for this OdooInstance.
        This method is called periodically by the operator's timer handler.
        
        This centralizes all time-based operations that need to be performed on the OdooInstance.
        """
        logging.debug(f"Performing periodic checks for {self.name}")
        
        # Check for scheduled upgrades
        self._check_scheduled_upgrade()
        
        # Check for upgrade job completion
        self._check_upgrade_job_completion()
        
        # Add any future periodic checks here
        # ...

    def _check_scheduled_upgrade(self):
        """
        Check if this instance has a scheduled upgrade that should be executed now.
        """
        logging.debug(f"Checking for scheduled upgrades for {self.name}")

        # Check if this is an upgrade request that should be executed now
        if self._is_upgrade_request() and self._should_execute_upgrade():
            logging.info(f"Executing scheduled upgrade for {self.name}")
            self._handle_upgrade()
        else:
            logging.debug(f"No scheduled upgrade to execute for {self.name}")

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

    def _check_upgrade_job_completion(self):
        """Check if the upgrade job has completed and handle completion tasks."""
        # Skip if there's no upgrade job at all
        if not self.upgrade_job.resource or self.upgrade_job.is_completed:
            return
            
        logging.debug(f"Checking upgrade job completion for {self.name}")

        try:
            self.upgrade_job.handle_completion()
        except Exception as e:
            logging.error(f"Error in upgrade job completion check for {self.name}: {e}")
            return
            
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

            except client.exceptions.ApiException as e:
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
