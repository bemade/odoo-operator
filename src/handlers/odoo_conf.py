from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing
import base64
from passlib.context import CryptContext

crypt_context = CryptContext(
    schemes=["pbkdf2_sha512", "plaintext"],
    deprecated=["plaintext"],
    pbkdf2_sha512__rounds=600_000,
)


class OdooConf(ResourceHandler):
    """Manages the Odoo configuration file ConfigMap."""

    def __init__(self, handler):
        super().__init__(handler)
        self.odoo_user_secret = handler.odoo_user_secret

    def _read_resource(self):
        return client.CoreV1Api().read_namespaced_config_map(
            name=f"{self.name}-odoo-conf",
            namespace=self.namespace,
        )

    @update_if_exists
    def handle_create(self):
        configmap = self._get_resource_body()
        self._resource = client.CoreV1Api().create_namespaced_config_map(
            namespace=self.namespace,
            body=configmap,
        )

    @create_if_missing
    def handle_update(self):
        configmap = self._get_resource_body()
        self._resource = client.CoreV1Api().patch_namespaced_config_map(
            name=f"{self.name}-odoo-conf",
            namespace=self.namespace,
            body=configmap,
        )

    def _get_resource_body(self):
        metadata = client.V1ObjectMeta(
            owner_references=[self.owner_reference],
            name=f"{self.name}-odoo-conf",
        )
        config_options = {
            "data_dir": "/var/lib/odoo",
            "proxy_mode": "True",
            "addons_path": "/mnt/extra-addons",
            "db_user": base64.b64decode(
                self.odoo_user_secret.resource.data["username"]
            ).decode(),
        }
        admin_pw = self.spec.get("adminPassword", "")
        if admin_pw:
            admin_pw = crypt_context.hash(admin_pw)
            config_options.update(admin_passwd=admin_pw)
        config_options.update(self.spec.get("configOptions", {}))
        conf_text = "[options]\n"
        for key, value in config_options.items():
            conf_text += f"{key} = {value}\n"
        return client.V1ConfigMap(metadata=metadata, data={"odoo.conf": conf_text})
