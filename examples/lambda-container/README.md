# Lambda container

An ordinary HTTP server in a container, run on Lambda behind a public function
URL, on the [serverless platform](../aws-platform). This is the reference
example of a container workload with class `aws-lambda`.
[`workload.yaml`](workload.yaml) declares one container under `containers:`,
with the image supplied at deploy time; `args` that the server needs; a service
port whose `targetPort` the server listens on; and an `http` trigger with
`auth: none`.

[`app/server.py`](app/server.py) knows nothing about Lambda. It is a Python
standard-library HTTP server that takes its port and greeting from the command
line. What makes the image work on Lambda is the [AWS Lambda Web
Adapter](https://github.com/awslabs/aws-lambda-web-adapter), which the
[`Dockerfile`](app/Dockerfile) copies in as a Lambda extension, pinned to
release 0.9.1. The adapter answers Lambda's runtime API and forwards each
invocation to the server as an HTTP request.

The [Lambda module](../modules/README.md#lambda-workloads) turns the workload
into an image function:

- the container's `args` become the function's image config command (and
  `command`, had it one, the entry point);
- the service's `targetPort` becomes the environment variable `AWS_LWA_PORT`,
  which tells the adapter where the server listens.

Setting `AWS_LWA_PORT` alone does not make an arbitrary image run on Lambda.
The image needs the adapter, or another integration with Lambda's runtime API,
such as an AWS Lambda base image with a handler like
[`examples/lambda-api`](../lambda-api). Without one, the function fails to
start whatever its environment says.

## Deploy

Push the image to an ECR repository in the platform's region, then deploy it by
tag; openporch pins the tag to its digest when it accepts the deployment.
Lambda accepts only a single-platform image manifest, so build without
provenance attestations:

```sh
registry=111122223333.dkr.ecr.eu-north-1.amazonaws.com
aws ecr get-login-password | docker login --username AWS --password-stdin "$registry"
docker build --platform linux/amd64 --provenance=false -t "$registry/lambda-container:1" app
docker push "$registry/lambda-container:1"
porch workload deploy -f workload.yaml --image "main=$registry/lambda-container:1" --wait
```

With `OPENPORCH_APP` and `OPENPORCH_ENV` set, the function's URL is the
workload's `url` output:

```sh
URL=$(porch get active-resources workload/modules.lambda-container -o json | jq -r .outputs.url)
curl "$URL"         # {"message": "Hello from a container on Lambda", "port": 8080}
curl "$URL/health"  # {"status": "ok"}
```

The greeting and the port come from the workload's `args`. The image's own
default arguments name port 3000 and another greeting, so this answer shows the
override took effect.

## Test

```sh
python3 -m unittest discover examples/lambda-container/app
```

`crates/server/src/tests/e2e.rs` deploys this file to a server with fake
drivers and definitions shaped like the platform's. `just e2e-aws`
([e2e](../../docs/e2e.md)) deploys it to a real AWS account, checks the
function's image config and `AWS_LWA_PORT`, and calls its URL.
