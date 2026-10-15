"""Tests for the order worker and the secret helper, with fake AWS clients.

Run with `python3 -m unittest discover examples/lambda-order-worker/app`.
"""

import decimal
import hashlib
import hmac
import json
import os
import unittest
from unittest import mock

import app
import secret_refs


class FakeTable:
    def __init__(self):
        self.items = {}

    def put_item(self, Item):
        self.items[Item["id"]] = Item

    def scan(self, Select, ExclusiveStartKey=None):
        # Pages of two items, like a scan that stops at 1 MB.
        assert Select == "COUNT"
        ids = sorted(self.items)
        start = ids.index(ExclusiveStartKey["id"]) + 1 if ExclusiveStartKey else 0
        page = ids[start:start + 2]
        result = {"Count": len(page)}
        if start + 2 < len(ids):
            result["LastEvaluatedKey"] = {"id": page[-1]}
        return result


class FakeSsm:
    def __init__(self, values):
        self.values = values
        self.calls = 0

    def get_parameter(self, Name, WithDecryption):
        assert WithDecryption
        self.calls += 1
        return {"Parameter": {"Value": self.values[Name]}}


class FakeSecretsManager:
    def get_secret_value(self, SecretId):
        return {"SecretString": {"openporch/shop-dev-order-worker/SIGNING_KEY": "sm-key"}[SecretId]}


def message(body, key):
    signature = hmac.new(key, body.encode(), hashlib.sha256).hexdigest()
    return {"messageId": "m", "body": body, "messageAttributes": {"signature": {"stringValue": signature}}}


class WorkerTest(unittest.TestCase):
    def test_signed_orders_are_stored_and_others_dropped(self):
        table = FakeTable()
        key = b"signing-key"
        event = {"Records": [
            message(json.dumps({"id": 1, "total": 30}), key),
            message(json.dumps({"id": 2}), b"another-key"),
            message("not json", key),
            message(json.dumps({"total": 5}), key),
        ]}
        with self.assertLogs(level="INFO") as logs:
            self.assertEqual(app.handler(event, None, table=table, signing_key=key), {"stored": 1})
        self.assertEqual(table.items, {"1": {"id": "1", "total": 30}})
        self.assertIn("INFO:root:stored order 1", logs.output)

    def test_decimal_totals_are_stored_as_decimals(self):
        table = FakeTable()
        key = b"signing-key"
        event = {"Records": [message(json.dumps({"id": "9", "total": 30.5}), key)]}
        self.assertEqual(app.handler(event, None, table=table, signing_key=key), {"stored": 1})
        self.assertEqual(table.items["9"]["total"], decimal.Decimal("30.5"))

    def test_a_signature_that_is_not_ascii_is_dropped(self):
        record = message(json.dumps({"id": 3}), b"signing-key")
        record["messageAttributes"]["signature"]["stringValue"] = "\u00e9"
        event = {"Records": [record]}
        self.assertEqual(app.handler(event, None, table=FakeTable(), signing_key=b"signing-key"), {"stored": 0})

    def test_the_nightly_report_counts_every_page(self):
        table = FakeTable()
        for i in range(5):
            table.put_item(Item={"id": str(i)})
        self.assertEqual(app.handler({"task": "report"}, None, table=table), {"orders": 5})

    def test_the_signing_key_is_read_through_its_reference(self):
        ref = "/openporch/shop-dev-order-worker/SIGNING_KEY"
        ssm = FakeSsm({ref: "signing-key"})
        with mock.patch.dict(os.environ, {"SIGNING_KEY_SECRET_REF": ref}), \
                mock.patch.object(secret_refs, "_client", return_value=ssm):
            secret_refs._cache.clear()
            event = {"Records": [message(json.dumps({"id": 7}), b"signing-key")]}
            self.assertEqual(app.handler(event, None, table=FakeTable()), {"stored": 1})


class SecretRefsTest(unittest.TestCase):
    def setUp(self):
        secret_refs._cache.clear()

    def test_parameters_are_read_and_kept_until_the_ttl_passes(self):
        ref = "/openporch/shop-dev-order-worker/SIGNING_KEY"
        ssm = FakeSsm({ref: "v1"})
        clock = [0.0]
        with mock.patch.dict(os.environ, {"SIGNING_KEY_SECRET_REF": ref}):
            read = lambda: secret_refs.secret("SIGNING_KEY", ssm=ssm, now=lambda: clock[0])
            self.assertEqual(read(), "v1")
            ssm.values[ref] = "v2"
            clock[0] = secret_refs.TTL_SECONDS - 1
            self.assertEqual(read(), "v1")
            clock[0] = secret_refs.TTL_SECONDS + 1
            self.assertEqual(read(), "v2")
        self.assertEqual(ssm.calls, 2)

    def test_secrets_manager_references_are_names_without_a_slash(self):
        ref = "openporch/shop-dev-order-worker/SIGNING_KEY"
        with mock.patch.dict(os.environ, {"SIGNING_KEY_SECRET_REF": ref}):
            self.assertEqual(secret_refs.secret("SIGNING_KEY", secretsmanager=FakeSecretsManager()), "sm-key")

    def test_a_variable_that_is_not_a_secret_has_no_reference(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(KeyError, "SIGNING_KEY_SECRET_REF"):
                secret_refs.secret("SIGNING_KEY", ssm=FakeSsm({}))

    def test_the_value_never_reaches_the_environment(self):
        ref = "/openporch/shop-dev-order-worker/SIGNING_KEY"
        with mock.patch.dict(os.environ, {"SIGNING_KEY_SECRET_REF": ref}):
            secret_refs.secret("SIGNING_KEY", ssm=FakeSsm({ref: "s3cr3t"}))
            self.assertNotIn("s3cr3t", "".join(os.environ.values()))


if __name__ == "__main__":
    unittest.main()
