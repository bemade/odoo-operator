"""
Custom webhook server implementation that uses service mode for Kubernetes webhook configurations.

This server extends Kopf's WebhookServer but yields a service configuration 
instead of a URL configuration, making it more suitable for in-cluster deployments.
"""

import logging
import kopf
import aiohttp.web
import asyncio
import base64
from conversion import convert_odoo_instance

logger = logging.getLogger(__name__)


class ServiceModeWebhookServer(kopf.WebhookServer):
    """
    A webhook server that uses service mode for Kubernetes webhook configurations.

    This server extends Kopf's WebhookServer but yields a service configuration
    instead of a URL configuration. This is more suitable for in-cluster deployments
    as it allows Kubernetes to route webhook requests through the service instead
    of using a URL.

    The service namespace and name are configurable via parameters:
    - service_namespace: The namespace where the service is deployed
    - service_name: The name of the service

    Note: The 'addr' and 'host' parameters from the parent WebhookServer are ignored
    in service mode, as the service name and namespace are used instead for routing.
    The server will still bind to the specified port, but the hostname/address
    is not used in the webhook configuration.
    """

    def __init__(
        self,
        *,
        service_name,
        service_namespace,
        addr=None,
        port=None,
        path=None,
        host=None,
        cadata=None,
        cafile=None,
        cadump=None,
        context=None,
        insecure=False,
        certfile=None,
        pkeyfile=None,
        password=None,
        extra_sans=(),
        verify_mode=None,
        verify_cafile=None,
        verify_capath=None,
        verify_cadata=None,
    ):
        super().__init__(
            addr=addr,
            port=port,
            path=path,
            host=host,
            cadata=cadata,
            cafile=cafile,
            cadump=cadump,
            context=context,
            insecure=insecure,
            certfile=certfile,
            pkeyfile=pkeyfile,
            password=password,
            extra_sans=extra_sans,
            verify_mode=verify_mode,
            verify_cafile=verify_cafile,
            verify_capath=verify_capath,
            verify_cadata=verify_cadata,
        )
        self.service_name = service_name
        self.service_namespace = service_namespace

    async def __call__(self, fn):
        """
        Start the webhook server and yield a service configuration.

        This overrides the default implementation to yield a service configuration
        instead of a URL configuration. Unlike the parent WebhookServer, this method
        ignores the 'addr' and 'host' parameters and uses the service_name and
        service_namespace instead for the webhook configuration.

        Args:
            fn: The webhook function to call when a request is received.

        Yields:
            dict: A service configuration for Kubernetes webhook configurations.
            The configuration uses the service_name and service_namespace instead of
            a URL, which is more suitable for in-cluster deployments.
        """
        # Build SSL context and get CA data
        cadata, context = self._build_ssl()
        path = self.path.rstrip("/") if self.path else ""

        # Set up the web application
        app = self._setup_app(fn, path)
        runner = self._setup_runner(app)
        await runner.setup()

        try:
            # Start the server
            addr = self.addr or None
            port = self.port or self._allocate_free_port()
            site = self._setup_site(runner, addr, port, context)
            await site.start()

            # Log the server details
            schema = "http" if context is None else "https"
            listen_url = self._build_url(schema, addr or "*", port, self.path or "")
            logger.debug(f"Listening for webhooks at {listen_url}")

            # Create a service configuration instead of a URL configuration
            client_config = {
                "service": {
                    "namespace": self.service_namespace,
                    "name": self.service_name,
                    "path": path,
                    "port": port,
                }
            }

            if cadata is not None:
                client_config["caBundle"] = base64.b64encode(cadata).decode("ascii")

            logger.info(
                f"Using service mode for webhook configuration: {self.service_name}.{self.service_namespace}"
            )
            yield client_config
            await asyncio.Event().wait()
        finally:
            await runner.cleanup()

    def _setup_app(self, fn, path):
        """Set up the web application with both admission and conversion endpoints."""

        async def _serve_fn(request):
            return await self._serve(fn, request)

        async def _conversion_fn(request):
            return await self._handle_conversion(request)

        app = aiohttp.web.Application()
        app.add_routes([
            aiohttp.web.post("/convert", _conversion_fn),
            aiohttp.web.post(f"{path}/{{id:.*}}", _serve_fn),
        ])
        return app

    async def _handle_conversion(self, request: aiohttp.web.Request) -> aiohttp.web.Response:
        """Handle CRD conversion webhook requests."""
        review = await request.json()
        req = review["request"]
        desired = req["desiredAPIVersion"]

        converted = []
        for obj in req["objects"]:
            kind = obj.get("kind", "")
            if kind == "OdooInstance":
                converted.append(convert_odoo_instance(obj, desired))
            else:
                obj["apiVersion"] = desired
                converted.append(obj)

        return aiohttp.web.json_response({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "ConversionReview",
            "response": {
                "uid": req["uid"],
                "result": {"status": "Success"},
                "convertedObjects": converted,
            },
        })

    def _setup_runner(self, app):
        """Set up the application runner for the webhook server."""
        return aiohttp.web.AppRunner(app, handle_signals=False)

    def _setup_site(self, runner, addr, port, context):
        """Set up the TCP site for the webhook server."""
        return aiohttp.web.TCPSite(
            runner, addr, port, ssl_context=context, reuse_port=True
        )
