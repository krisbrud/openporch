# Lambda API

A small HTTP API as a Lambda image function behind a public function URL, on
the [serverless platform](../aws-platform). [`workload.yaml`](workload.yaml)
declares an image function, with the image supplied at deploy time, and an
`http` trigger with `auth: none`. A URL is public only when a workload asks
for that, and `auth: iam` makes one that needs signed requests.

The image is built from the AWS Lambda Python base image, which includes the
runtime interface, so it needs no web adapter. For an existing HTTP server in a
container, see the Lambda Web Adapter in
[`examples/modules`](../modules/README.md#lambda-workloads).

## Deploy

Push the image to an ECR repository in the platform's region, then deploy it by
tag; openporch pins the tag to its digest when it accepts the deployment.
Lambda accepts only a single-platform image manifest, so build without
provenance attestations:

```sh
registry=111122223333.dkr.ecr.eu-north-1.amazonaws.com
aws ecr get-login-password | docker login --username AWS --password-stdin "$registry"
docker build --platform linux/amd64 --provenance=false -t "$registry/lambda-api:1" app
docker push "$registry/lambda-api:1"
porch workload deploy -f workload.yaml --image "main=$registry/lambda-api:1" --wait
```

With `OPENPORCH_APP` and `OPENPORCH_ENV` set, the function's URL is the
workload's `url` output:

```sh
URL=$(porch get active-resources workload/modules.lambda-api -o json | jq -r .outputs.url)
curl "$URL"         # {"message": "Hello from openporch"}
curl "$URL/health"  # {"status": "ok"}
```

## Test

```sh
python3 -m unittest discover examples/lambda-api/app
```

`crates/server/src/tests/e2e.rs` deploys this file to a server with fake
drivers and definitions shaped like the platform's.
