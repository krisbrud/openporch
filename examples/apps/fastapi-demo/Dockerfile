FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

WORKDIR /app

ENV UV_LINK_MODE=copy \
    UV_COMPILE_BYTECODE=1 \
    UV_PYTHON_DOWNLOADS=never

COPY pyproject.toml ./
COPY src ./src
RUN uv sync --no-dev

EXPOSE 8080

CMD ["uv", "run", "--no-dev", "uvicorn", "fastapi_demo.main:app", "--host", "0.0.0.0", "--port", "8080"]
