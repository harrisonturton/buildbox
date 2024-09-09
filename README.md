## `build-server`

Intended to be a simple, single-server implementation of the Bazel remote
execution API. Not functional yet.

## TLS configuration

To use self-signed certificates for testing:

```
openssl req -new -newkey rsa:4096 -x509 -sha256 -days 3650 -nodes -batch -subj "/CN=localhost" -out server.crt -keyout server.key
openssl req -new -newkey rsa:4096 -x509 -sha256 -days 3650 -nodes -batch -subj "/CN=client" -out client.crt -keyout client.key
```

The `server.crt` and `server.key` are used by the `build-server`, and the
`client.crt` and `client.key` are used by Bazel. For example, they can be added
to your `.bazelrc` like:

```
build --tls_certificate="server.crt"
build --tls_client_certificate="client.crt"
build --tls_client_key="client.key"
```