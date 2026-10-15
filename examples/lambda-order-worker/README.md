# Order worker

A Python function that stores signed orders from an SQS queue in a DynamoDB
table, and logs how many orders the table holds every night at 03:00 Oslo
time. It runs on the [serverless platform](../aws-platform) as a Lambda function
of class `aws-lambda`, and its [`workload.yaml`](workload.yaml) asks for:

- the table `orders`, whose name reaches the function as `TABLE`;
- the queue `inbox`, which triggers the function in batches of up to 10;
- a nightly schedule, whose event is `{"task": "report"}`;
- `SIGNING_KEY`, from the secret shared value `ORDER_SIGNING_KEY`.

The platform co-provisions a policy for the table and one for the queue and
attaches both to the function's role, so the function may write the table and
receive and delete the queue's messages without any IAM change by hand.

## Secrets at runtime

openporch never puts a secret into the function's environment. `SIGNING_KEY`
arrives as `SIGNING_KEY_SECRET_REF`, the name of an SSM SecureString parameter
only the function's role may read, and
[`app/secret_refs.py`](app/secret_refs.py) reads it at runtime:

```python
import secret_refs

key = secret_refs.secret("SIGNING_KEY")
```

A value is read once and kept for five minutes, so a changed secret reaches
running functions within that time after the next deployment writes it to the
parameter. Plain variables, such as `TABLE`, are ordinary environment
variables.

## Deploy

Create the secret shared value once per application; the value is written to
openporch's secret store:

```sh
curl -X POST -H "Authorization: Bearer $OPENPORCH_TOKEN" -H 'Content-Type: application/json' \
  "$OPENPORCH_URL/orgs/$OPENPORCH_ORG/apps/shop/values" \
  -d '{"key": "ORDER_SIGNING_KEY", "description": "Signs orders on the inbox queue",
       "secret_ref": {"value": "'"$(openssl rand -hex 32)"'"}}'
```

Then build the zip, upload it under a key that names its content, and deploy
it with that artifact:

```sh
mkdir -p build
(cd app && zip -X -q ../build/order-worker.zip app.py secret_refs.py)
key="order-worker/$(sha256sum build/order-worker.zip | cut -c1-64).zip"
aws s3 cp build/order-worker.zip "s3://$ARTIFACT_BUCKET/$key"
porch workload deploy -f workload.yaml --artifact "main=s3://$ARTIFACT_BUCKET/$key" --wait
```

openporch pins the object's current version when it accepts the deployment.
[`ci/deploy.yml`](ci/deploy.yml) does the same from GitHub Actions on every push
to `main`.

To send an order, sign its body with the key:

```sh
body='{"id": "1001", "total": 30}'
signature=$(printf %s "$body" | openssl dgst -sha256 -hmac "$KEY" -r | cut -d' ' -f1)
aws sqs send-message --queue-url "$QUEUE_URL" --message-body "$body" \
  --message-attributes "signature={DataType=String,StringValue=$signature}"
```

## Test

```sh
python3 -m unittest discover examples/lambda-order-worker/app
```

The tests use fake AWS clients. `crates/server/src/tests/e2e.rs` deploys this
file to a server with fake drivers and definitions shaped like the platform's.
