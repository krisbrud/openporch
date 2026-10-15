"""Secret variables, read at runtime.

openporch never puts a secret into a function's environment. A variable NAME
that resolved from a secret arrives as NAME_SECRET_REF instead: the name of an
SSM SecureString parameter (it starts with `/`) or of a Secrets Manager secret.
`secret("NAME")` reads it with the function's own role and keeps it for
`TTL_SECONDS`, so a rotated secret is picked up within that time without a
redeploy.
"""

import os
import time

TTL_SECONDS = 300

_cache = {}


def secret(name, *, ssm=None, secretsmanager=None, now=time.monotonic):
    """The value of the secret variable `name`."""
    ref = os.environ.get(f"{name}_SECRET_REF")
    if not ref:
        raise KeyError(f"{name}_SECRET_REF is not set; is {name} a secret variable?")
    cached = _cache.get(ref)
    if cached is not None and now() - cached[1] < TTL_SECONDS:
        return cached[0]
    if ref.startswith("/"):
        client = ssm or _client("ssm")
        value = client.get_parameter(Name=ref, WithDecryption=True)["Parameter"]["Value"]
    else:
        client = secretsmanager or _client("secretsmanager")
        value = client.get_secret_value(SecretId=ref)["SecretString"]
    _cache[ref] = (value, now())
    return value


def _client(service):
    # The Lambda Python runtime includes boto3.
    import boto3

    return boto3.client(service)
