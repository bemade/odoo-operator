from kubernetes import client
from .resource_handler import ResourceHandler, update_if_exists, create_if_missing


class IngressRoute(ResourceHandler):
    """Base class for Traefik IngressRoute resources."""

    def __init__(self, handler):
        super().__init__(handler)
        self.operator_ns = handler.operator_ns
        self.tls_cert = handler.tls_cert

    def _get_route_config(self):
        raise NotImplementedError("Subclasses must implement this method.")

    def _read_resource(self):
        return client.CustomObjectsApi().get_namespaced_custom_object(
            group="traefik.io",
            version="v1alpha1",
            namespace=self.namespace,
            plural="ingressroutes",
            name=self._get_route_name(),
        )

    def _build_ingress_route_spec(self):
        """Build the IngressRoute spec based on configuration from child class."""

        # Build TLS configuration
        tls = {"secretName": self.tls_cert.resource.get("metadata", {}).get("name")}

        # Get hostnames from spec
        hostnames = self.spec.get("ingress", {}).get("hosts", [])

        # Build match rule
        match_rule = " || ".join(f"Host(`{hostname}`)" for hostname in hostnames or [])

        # Return the complete spec
        return {
            "apiVersion": "traefik.io/v1alpha1",
            "kind": "IngressRoute",
            "metadata": {
                "name": self._get_route_name(),
                "ownerReferences": [self.owner_reference],
            },
            "spec": {
                "entryPoints": ["websecure"],
                "routes": [
                    {
                        "kind": "Rule",
                        "match": match_rule,
                        "services": [
                            {
                                "kind": "Service",
                                "name": self.name,
                                "namespace": self.namespace,
                                "passHostHeader": True,
                                "port": 8069,
                                "scheme": "http",
                            },
                        ],
                    },
                    {
                        "kind": "Rule",
                        "match": match_rule + " && PathPrefix(`/websocket`)",
                        "services": [
                            {
                                "kind": "Service",
                                "name": self.name,
                                "namespace": self.namespace,
                                "passHostHeader": True,
                                "port": 8072,
                                "scheme": "http",
                            },
                        ],
                    },
                ],
                "tls": tls,
            },
        }

    @update_if_exists
    def handle_create(self):
        # Build the ingress route spec
        body = self._build_ingress_route_spec()

        # Create the resource
        self._resource = client.CustomObjectsApi().create_namespaced_custom_object(
            group="traefik.io",
            version="v1alpha1",
            namespace=self.namespace,
            plural="ingressroutes",
            body=body,
        )

    @create_if_missing
    def handle_update(self):
        # Build the updated ingress route spec
        updated_spec = self._build_ingress_route_spec()

        # Update the resource
        self._resource = client.CustomObjectsApi().patch_namespaced_custom_object(
            group="traefik.io",
            version="v1alpha1",
            namespace=self.namespace,
            plural="ingressroutes",
            name=self._get_route_name(),
            body=updated_spec,
        )

    def _get_route_name(self):
        """Return the name of the ingress route."""
        raise NotImplementedError()
