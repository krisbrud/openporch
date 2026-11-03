# An ordinary HTTP server image. The AWS Lambda Web Adapter, copied in as a
# Lambda extension, is what lets it run on Lambda: it answers Lambda's runtime
# API and forwards each invocation to the server as an HTTP request on
# AWS_LWA_PORT, which openporch sets from the service's `targetPort`. Without
# the adapter the image would not run on Lambda, whatever AWS_LWA_PORT says.
#
# Build for the function's architecture, x86_64 by default:
#
#   docker build --platform linux/amd64 --provenance=false -t lambda-container app
FROM public.ecr.aws/docker/library/python:3.12-slim
# Pinned to a release; see examples/modules/README.md.
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
ENV PYTHONUNBUFFERED=1
COPY server.py /app/server.py
ENTRYPOINT ["python3", "/app/server.py"]
# The workload's `args` replace these. If they did not, the server would listen
# on 3000 rather than the adapter's port, and greet differently.
CMD ["--port", "3000", "--greeting", "Hello from the image's default arguments"]
