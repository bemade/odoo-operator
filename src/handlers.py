from kubernetes import client
import secrets
import os
import psycopg2
import yaml
import logging
from passlib.context import CryptContext


logger = logging.getLogger("odoo-operator")
crypt_context = CryptContext(
    schemes=["pbkdf2_sha512", "plaintext"],
    deprecated=["plaintext"],
    pbkdf2_sha512__rounds=600_000,
)


class OdooHandler:
    def __init__(self, body, **kwargs):
        self.body = body
        self.spec = body.get("spec", {})
        self.meta = body.get("meta", body.get("metadata"))
        self.namespace = self.meta.get("namespace")
        self.operator_ns = os.environ["OPERATOR_NAMESPACE"]
        self.name = self.meta.get("name")
        self.uid = self.meta.get("uid")
        with open("/etc/odoo/instance-defaults.yaml") as f:
            self.defaults = yaml.safe_load(f)
        self._pull_secret = None
        self._odoo_user_secret = None
        self._filestore_pvc = None
        self._odoo_conf = None
        self._postgres_secret = None
        self._tls_cert = None
        self._deployment = None
        self._service = None
        self._ingress_route_http = None
        self._ingress_route_https = None
        self._ingress_route_websocket = None

    def on_create(self):
        self._init_pull_secret()
        self._init_odoo_user_secret()
        self._init_filestore_pvc()
        self._init_odoo_conf()
        self._init_tls_cert()
        self._init_deployment()
        self._init_service()
        self._init_ingress_route_http()
        self._init_ingress_route_https()
        self._init_ingress_route_websocket()

    def on_update(self):
        pass

    def on_delete(self):
        # self._scale_down_deployment()
        self._delete_odoo_db_user()

    def _scale_down_deployment(self):
        if not self.deployment:
            return
        self.deployment.spec.replicas = 0
        client.AppsV1Api().patch_namespaced_deployment(
            name=self.name,
            namespace=self.namespace,
            body=self.deployment,
        )

    @property
    def owner_reference(self):
        return client.V1OwnerReference(
            api_version="odoo.bemade.org/v1",
            kind="OdooInstance",
            name=self.name,
            uid=self.uid,
            block_owner_deletion=True,
        )

    @property
    def pull_secret(self):
        if not self._pull_secret:
            try:
                self._pull_secret = client.CoreV1Api().read_namespaced_secret(
                    name=f"{self.spec.get('imagePullSecret')}",
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Secret not found, that's fine
                    pass
                else:
                    raise
        return self._pull_secret

    def _init_pull_secret(self):
        if not self.pull_secret:
            orig_secret = client.CoreV1Api().read_namespaced_secret(
                name=f"{self.spec.get('imagePullSecret')}",
                namespace=self.operator_ns,
            )
            secret = client.V1Secret(
                metadata=client.V1ObjectMeta(
                    name=f"{self.spec.get('imagePullSecret')}",
                    owner_references=[self.owner_reference],
                ),
                type="Obscure",
                data=orig_secret.data,
            )
            self._pull_secret = client.CoreV1Api().create_namespaced_secret(
                namespace=self.namespace,
                body=secret,
            )

    @property
    def odoo_user_secret(self):
        if not self._odoo_user_secret:
            try:
                self._odoo_user_secret = client.CoreV1Api().read_namespaced_secret(
                    name=f"{self.name}-odoo-user",
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Secret not found, that's fine
                    pass
                else:
                    raise
        return self._odoo_user_secret

    def _init_odoo_user_secret(self):
        if not self.odoo_user_secret:
            username, password = self._create_odoo_db_user()
            self._odoo_user_secret = self._create_odoo_user_secret(username, password)

    def _create_odoo_db_user(self):
        """
        Create the Odoo database user. If it already exists, drop it first.

        Returns:
            tuple: (username, password)
        """
        username = f"odoo.{self.namespace}.{self.name}"
        password = secrets.token_urlsafe(32)

        db_host = os.environ["DB_HOST"]
        db_port = os.environ["DB_PORT"]
        db_superuser = os.environ["DB_ADMIN_USER"]
        db_superuser_password = os.environ["DB_ADMIN_PASSWORD"]

        # Delete the user if it already exists
        self._delete_odoo_db_user()

        # Create the database user first

        with psycopg2.connect(
            host=db_host,
            port=db_port,
            user=db_superuser,
            password=db_superuser_password,
        ) as conn:
            with conn.cursor() as cur:
                cur.execute(
                    f"CREATE ROLE \"{username}\" WITH PASSWORD '{password}' CREATEDB LOGIN"
                )

        return username, password

    def _delete_odoo_db_user(self):
        db_host = os.environ["DB_HOST"]
        db_port = os.environ["DB_PORT"]
        db_superuser = os.environ["DB_ADMIN_USER"]
        db_superuser_password = os.environ["DB_ADMIN_PASSWORD"]
        username = f"odoo.{self.namespace}.{self.name}"
        conn = psycopg2.connect(
            host=db_host,
            port=db_port,
            user=db_superuser,
            password=db_superuser_password,
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
                cur.execute(f"DROP DATABASE {database}")

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

    @property
    def filestore_pvc(self):
        if not self._filestore_pvc:
            try:
                self._filestore_pvc = (
                    client.CoreV1Api().read_namespaced_persistent_volume_claim(
                        name=f"{self.name}-filestore-pvc",
                        namespace=self.namespace,
                    )
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # PVC not found, that's fine
                    pass
                else:
                    raise
        return self._filestore_pvc

    def _init_filestore_pvc(self):
        if not self.filestore_pvc:
            spec = self.spec.get("filestore", {})
            with open("/etc/odoo/instance-defaults.yaml") as f:
                defaults = yaml.safe_load(f)
            size = spec.get("size", defaults.get("filestoreSize", "2Gi"))
            storage_class = spec.get(
                "storageClass", defaults.get("storageClass", "standard")
            )
            metadata = client.V1ObjectMeta(
                name=f"{self.name}-filestore-pvc",
                owner_references=[self.owner_reference],
            )
            pvc = client.V1PersistentVolumeClaim(
                metadata=metadata,
                spec=client.V1PersistentVolumeClaimSpec(
                    access_modes=["ReadWriteOnce"],
                    storage_class_name=storage_class,
                    resources=client.V1ResourceRequirements(requests={"storage": size}),
                ),
            )
            self._filestore_pvc = (
                client.CoreV1Api().create_namespaced_persistent_volume_claim(
                    namespace=self.namespace,
                    body=pvc,
                )
            )

    @property
    def odoo_conf(self):
        if not self._odoo_conf:
            try:
                self._odoo_conf = client.CoreV1Api().read_namespaced_config_map(
                    name=f"{self.name}-odoo-conf",
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # ConfigMap not found, that's fine
                    pass
                else:
                    raise
        return self._odoo_conf

    def _init_odoo_conf(self):
        if not self.odoo_conf:
            metadata = client.V1ObjectMeta(
                owner_references=[self.owner_reference],
                name=f"{self.name}-odoo-conf",
            )
            config_options = {
                "data_dir": "/var/lib/odoo",
                "proxy_mode": "True",
                "addons_path": "/mnt/extra-addons",
            }
            admin_pw = self.spec.get("adminPassword", "")
            if admin_pw:
                admin_pw = crypt_context.hash(admin_pw)
                config_options.update(admin_password=admin_pw)
            config_options.update(self.spec.get("configOptions", {}))

            conf_text = "[options]\n"
            for key, value in config_options.items():
                conf_text += f"{key} = {value}\n"
            configmap = client.V1ConfigMap(
                metadata=metadata, data={"odoo.conf": conf_text}
            )
            self._odoo_conf = client.CoreV1Api().create_namespaced_config_map(
                namespace=self.namespace,
                body=configmap,
            )

    @property
    def tls_cert(self):
        if not self._tls_cert:
            try:
                self._tls_cert = client.CoreV1Api().read_namespaced_secret(
                    name=f"{self.spec.get('ingress').get('host')}-cert",
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Secret not found, that's fine
                    pass
                else:
                    raise
        return self._tls_cert

    def _init_tls_cert(self):
        if not self.tls_cert:
            host = self.spec.get("ingress").get("host")
            apiVersion = "cert-manager.io/v1"
            metadata = client.V1ObjectMeta(
                name=f"{host}-cert",
                owner_references=[self.owner_reference],
            )
            cert_spec = {
                "secretName": f"{host}-cert",
                "dnsNames": [host],
                "issuerRef": {
                    "name": self.spec.get("ingress").get("issuer"),
                    "kind": "ClusterIssuer",
                },
            }
            cert_definition = {
                "apiVersion": apiVersion,
                "kind": "Certificate",
                "metadata": metadata,
                "spec": cert_spec,
            }
            self._tls_cert = client.CustomObjectsApi().create_namespaced_custom_object(
                group="cert-manager.io",
                version="v1",
                namespace=self.namespace,
                plural="certificates",
                body=cert_definition,
            )

    @property
    def deployment(self):
        if not self._deployment:
            try:
                self._deployment = client.AppsV1Api().read_namespaced_deployment(
                    name=self.name,
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Deployment not found, that's fine
                    pass
                else:
                    raise
        return self._deployment

    def _init_deployment(self):
        if not self.deployment:
            db_host = os.environ["DB_HOST"]
            db_port = os.environ["DB_PORT"]

            image = self.spec.get("image", self.defaults.get("odooImage", "odoo:18.0"))

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
                replicas=1,
                selector=client.V1LabelSelector(match_labels={"app": self.name}),
                strategy={"type": "Recreate"},
                template=client.V1PodTemplateSpec(
                    metadata=client.V1ObjectMeta(
                        labels={"app": self.name},
                    ),
                    spec=client.V1PodSpec(
                        **pull_secret,
                        volumes=[
                            client.V1Volume(
                                name=f"filestore",
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
                        ],
                        security_context=client.V1PodSecurityContext(
                            run_as_user=1001,
                            run_as_group=1001,
                            fs_group=1001,
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
                                volume_mounts=[
                                    client.V1VolumeMount(
                                        name="filestore",
                                        mount_path="/var/lib/odoo",
                                    ),
                                    client.V1VolumeMount(
                                        name="odoo-conf",
                                        mount_path="/etc/odoo",
                                    ),
                                ],
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
                                    initial_delay_seconds=60,
                                    period_seconds=20,
                                    timeout_seconds=10,
                                    success_threshold=1,
                                    failure_threshold=6,
                                ),
                                readiness_probe=client.V1Probe(
                                    http_get=client.V1HTTPGetAction(
                                        path="/web/health",
                                        port=8069,
                                    ),
                                    initial_delay_seconds=60,
                                    period_seconds=20,
                                    timeout_seconds=10,
                                    success_threshold=1,
                                    failure_threshold=6,
                                ),
                            )
                        ],
                    ),
                ),
            )
            self._deployment = client.AppsV1Api().create_namespaced_deployment(
                namespace=self.namespace,
                body=client.V1Deployment(
                    metadata=metadata,
                    spec=spec,
                ),
            )

    @property
    def service(self):
        if not self._service:
            try:
                self._service = client.CoreV1Api().read_namespaced_service(
                    name=self.name,
                    namespace=self.namespace,
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Service not found, that's fine
                    pass
                else:
                    raise
        return self._service

    def _init_service(self):
        metadata = client.V1ObjectMeta(
            name=self.name,
            owner_references=[self.owner_reference],
            labels={"app": self.name},
        )
        service = client.V1Service(
            metadata=metadata,
            spec=client.V1ServiceSpec(
                selector={"app": self.name},
                ports=[
                    client.V1ServicePort(
                        port=8069,
                        target_port=8069,
                        name="http",
                    ),
                    client.V1ServicePort(
                        port=8072,
                        target_port=8072,
                        name="websocket",
                    ),
                ],
                type="ClusterIP",
            ),
        )
        self._service = client.CoreV1Api().create_namespaced_service(
            namespace=self.namespace,
            body=service,
        )

    @property
    def ingress_route_http(self):
        if not self._ingress_route_http:
            try:
                self._ingress_route_http = (
                    client.CustomObjectsApi().get_namespaced_custom_object(
                        group="traefik.io",
                        version="v1alpha1",
                        namespace=self.namespace,
                        plural="ingressroutes",
                        name=f"{self.name}-http",
                    )
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Ingress not found, that's fine
                    pass
                else:
                    raise
        return self._ingress_route_http

    def _create_ingress_route(self, suffix, entrypoint, port, middlewares):
        return client.CustomObjectsApi().create_namespaced_custom_object(
            group="traefik.io",
            version="v1alpha1",
            namespace=self.namespace,
            plural="ingressroutes",
            body={
                "apiVersion": "traefik.io/v1alpha1",
                "kind": "IngressRoute",
                "metadata": {
                    "name": f"{self.name}-{suffix}",
                    "ownerReferences": [self.owner_reference],
                },
                "spec": {
                    "entryPoints": [entrypoint],
                    "routes": [
                        {
                            "kind": "Rule",
                            "match": f"Host(`{self.name}.cluster.local`)",
                            "middlewares": middlewares,
                            "services": [
                                {
                                    "kind": "Service",
                                    "name": self.name,
                                    "namespace": self.namespace,
                                    "passHostHeader": True,
                                    "port": port,
                                    "scheme": "http",
                                }
                            ],
                        }
                    ],
                },
            },
        )

    def _init_ingress_route_http(self):
        if not self.ingress_route_http:
            self._ingress_route_http = self._create_ingress_route(
                suffix="http",
                entrypoint="web",
                port=8069,
                middlewares=[
                    {
                        "name": "redirect-https",
                        "namespace": self.operator_ns,
                    },
                ],
            )

    @property
    def ingress_route_https(self):
        if not self._ingress_route_https:
            try:
                self._ingress_route_https = (
                    client.CustomObjectsApi().get_namespaced_custom_object(
                        group="traefik.io",
                        version="v1alpha1",
                        namespace=self.namespace,
                        plural="ingressroutes",
                        name=f"{self.name}-https",
                    )
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Ingress not found, that's fine
                    pass
                else:
                    raise
        return self._ingress_route_https

    def _init_ingress_route_https(self):
        if not self.ingress_route_https:
            self._ingress_route_https = self._create_ingress_route(
                suffix="https",
                entrypoint="websecure",
                port=8069,
                middlewares=[],
            )

    @property
    def ingress_route_websocket(self):
        if not self._ingress_route_websocket:
            try:
                self._ingress_route_websocket = (
                    client.CustomObjectsApi().get_namespaced_custom_object(
                        group="traefik.io",
                        version="v1alpha1",
                        namespace=self.namespace,
                        plural="ingressroutes",
                        name=f"{self.name}-websocket",
                    )
                )
            except client.exceptions.ApiException as e:
                if e.status == 404:
                    # Ingress not found, that's fine
                    pass
                else:
                    raise
        return self._ingress_route_websocket

    def _init_ingress_route_websocket(self):
        if not self.ingress_route_websocket:
            self._ingress_route_websocket = self._create_ingress_route(
                suffix="websocket",
                entrypoint="websecure",
                port=8072,
                middlewares=[
                    {
                        "name": "remove-prefix",
                        "namespace": self.operator_ns,
                    },
                ],
            )
