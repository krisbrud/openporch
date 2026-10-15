# The AWS base image includes the Lambda runtime interface, so no adapter is
# needed. Build for the function's architecture, x86_64 by default:
#
#   docker build --platform linux/amd64 --provenance=false -t lambda-api app
FROM public.ecr.aws/lambda/python:3.12
COPY app.py ${LAMBDA_TASK_ROOT}/
CMD ["app.handler"]
