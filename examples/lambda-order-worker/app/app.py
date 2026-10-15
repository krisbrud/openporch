"""The order worker.

Queue messages are orders, JSON objects with an `id`, signed with HMAC-SHA256
over the message body using the secret `SIGNING_KEY`; the signature is the
message attribute `signature`, in hex. Valid orders are written to the table
`TABLE`, others are logged and dropped. The nightly schedule's event,
`{"task": "report"}`, logs how many orders the table holds.

A message that cannot be an order is dropped, so it never blocks the queue. If
storing a valid order fails, the handler raises and SQS retries the whole
batch; writes are keyed by the order's `id`, so storing an order again is
harmless.
"""

import decimal
import hashlib
import hmac
import json
import logging
import os

import secret_refs

log = logging.getLogger()
try:
    log.setLevel(os.environ.get("LOG_LEVEL", "info").upper())
except ValueError:
    log.setLevel(logging.INFO)

_table = None


def handler(event, context, *, table=None, signing_key=None):
    table = table or _orders_table()
    if "Records" in event:
        key = signing_key or secret_refs.secret("SIGNING_KEY").encode()
        return {"stored": sum(_store(r, table, key) for r in event["Records"])}
    if event.get("task") == "report":
        count = _count(table)
        log.info("the table holds %d orders", count)
        return {"orders": count}
    raise ValueError(f"unexpected event: {sorted(event)}")


def _store(record, table, key):
    body = record["body"]
    signature = record.get("messageAttributes", {}).get("signature", {}).get("stringValue", "")
    expected = hmac.new(key, body.encode(), hashlib.sha256).hexdigest()
    if not hmac.compare_digest(signature.encode(), expected.encode()):
        log.warning("dropped message %s: bad signature", record.get("messageId"))
        return 0
    try:
        # DynamoDB stores numbers as decimals and refuses floats.
        order = json.loads(body, parse_float=decimal.Decimal)
        order_id = str(order["id"])
    except (ValueError, KeyError, TypeError):
        log.warning("dropped message %s: not an order", record.get("messageId"))
        return 0
    table.put_item(Item={**order, "id": order_id})
    return 1


def _count(table):
    """All items, page by page: one scan reads at most 1 MB."""
    count, start = 0, {}
    while True:
        page = table.scan(Select="COUNT", **start)
        count += page["Count"]
        if "LastEvaluatedKey" not in page:
            return count
        start = {"ExclusiveStartKey": page["LastEvaluatedKey"]}


def _orders_table():
    global _table
    if _table is None:
        import boto3

        _table = boto3.resource("dynamodb").Table(os.environ["TABLE"])
    return _table
