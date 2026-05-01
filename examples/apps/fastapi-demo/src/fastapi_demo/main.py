import os

import psycopg
from fastapi import FastAPI
from fastapi.responses import JSONResponse

app = FastAPI(title="fastapi-demo")


@app.get("/")
def root() -> dict:
    return {"message": "hello", "service": "fastapi-demo"}


@app.get("/health")
def health() -> JSONResponse:
    url = os.environ.get("DATABASE_URL")
    if not url:
        return JSONResponse({"db": "missing DATABASE_URL"}, status_code=503)
    try:
        with psycopg.connect(url, connect_timeout=3) as conn:
            with conn.cursor() as cur:
                cur.execute("SELECT 1")
                cur.fetchone()
        return JSONResponse({"db": "ok"})
    except Exception as exc:
        return JSONResponse({"db": "error", "detail": str(exc)}, status_code=503)
