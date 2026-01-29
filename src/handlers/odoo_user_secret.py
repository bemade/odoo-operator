from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
from .postgres_clusters import get_cluster_for_instance
import secrets
import psycopg2
import base64


class OdooUserSecret(ResourceHandler):
    """Manages the Odoo user secret and database user."""

    def _read_resource(self):
        return client.CoreV1Api().read_namespaced_secret(
            name=f"{self.name}-odoo-user",
            namespace=self.namespace,
        )

    @update_if_exists
    def handle_create(self):
        username, password = self._create_odoo_db_user()
        self._resource = self._create_odoo_user_secret(username, password)

    @create_if_missing
    def handle_update(self):
        # We don't update the user secret as it's internal only
        return

    @property
    def username(self):
        if not self.resource:
            return None
        return base64.b64decode(self.resource.data.get("username")).decode("utf-8")

    @property
    def password(self):
        if not self.resource:
            return None
        return base64.b64decode(self.resource.data.get("password")).decode("utf-8")

    def handle_delete(self):
        self._delete_odoo_db_user()

    def _create_odoo_db_user(self):
        """
        Create the Odoo database user. If it already exists, drop it first.

        Returns:
            tuple: (username, password)
        """
        username = f"odoo.{self.namespace}.{self.name}"
        password = secrets.token_urlsafe(32)

        # Delete the user if it already exists
        self._delete_odoo_db_user()

        # Get the PostgreSQL cluster configuration for this instance
        pg_cluster = get_cluster_for_instance(self.spec)

        with psycopg2.connect(
            host=pg_cluster.host,
            port=pg_cluster.port,
            user=pg_cluster.admin_user,
            password=pg_cluster.admin_password,
        ) as conn:
            with conn.cursor() as cur:
                cur.execute(
                    f"CREATE ROLE \"{username}\" WITH PASSWORD '{password}' CREATEDB LOGIN"
                )

        return username, password

    def _delete_odoo_db_user(self):
        # Get the PostgreSQL cluster configuration for this instance
        pg_cluster = get_cluster_for_instance(self.spec)
        username = f"odoo.{self.namespace}.{self.name}"

        conn = psycopg2.connect(
            host=pg_cluster.host,
            port=pg_cluster.port,
            user=pg_cluster.admin_user,
            password=pg_cluster.admin_password,
            database="postgres",
        )
        conn.autocommit = True

        with conn.cursor() as cur:
            cur.execute(
                f"SELECT datname FROM pg_database WHERE datistemplate = false AND datdba = (SELECT usesysid FROM pg_user WHERE usename = '{username}')"
            )
            databases = [row[0] for row in cur.fetchall()]
            cur.execute(f"SELECT 1 FROM pg_roles WHERE rolname = '{username}'")
            drop = bool(cur.fetchone())

        for database in databases:
            with conn.cursor() as cur:
                cur.execute(f'DROP DATABASE "{database}"')

        if drop:
            with conn.cursor() as cur:
                cur.execute(f'DROP ROLE "{username}"')

        conn.close()

    def _create_odoo_user_secret(self, username, password):
        metadata = client.V1ObjectMeta(
            name=f"{self.name}-odoo-user", owner_references=[self.owner_reference]
        )
        secret = client.V1Secret(
            metadata=metadata,
            string_data={
                "username": username,
                "password": password,
            },
            type="Opaque",
        )
        return client.CoreV1Api().create_namespaced_secret(
            namespace=self.namespace,
            body=secret,
        )
