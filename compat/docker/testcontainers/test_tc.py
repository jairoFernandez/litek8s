"""Testcontainers (Python) against litek8s: random published ports, log and
port wait strategies, exec, file copy and the Ryuk reaper (which mounts the
Docker socket into its container and removes everything when the session
ends).

    pip install "testcontainers[postgres,redis]" psycopg2-binary redis pytest
    DOCKER_HOST=unix://$HOME/.litek8s/docker.sock pytest -q test_tc.py
"""
import io
import tarfile

import docker
import psycopg2
import redis
from testcontainers.core.container import DockerContainer
from testcontainers.core.waiting_utils import wait_for_logs
from testcontainers.postgres import PostgresContainer
from testcontainers.redis import RedisContainer


def test_postgres():
    with PostgresContainer("postgres:16-alpine") as pg:
        conn = psycopg2.connect(host=pg.get_container_host_ip(), port=pg.get_exposed_port(5432), user=pg.username, password=pg.password, dbname=pg.dbname)
        cur = conn.cursor()
        cur.execute("select 40 + 2")
        assert cur.fetchone()[0] == 42
        conn.close()


def test_redis():
    with RedisContainer("redis:7-alpine") as r:
        client = r.get_client()
        client.set("k", "v")
        assert client.get("k") == b"v"


def test_generic_logs_exec_and_copy():
    with DockerContainer("alpine:3.20").with_command("sh -c 'echo booted; sleep 300'").with_exposed_ports(8080) as c:
        wait_for_logs(c, "booted", timeout=30)
        code, out = c.exec(["sh", "-c", "echo hi && id -u"])
        assert code == 0 and out.decode().split() == ["hi", "0"]
        # Copy a file in (PUT archive) and back out (GET archive).
        raw = c.get_wrapped_container()
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as t:
            data = b"payload"
            info = tarfile.TarInfo("hello.txt")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))
        assert raw.put_archive("/tmp", buf.getvalue())
        stream, stat = raw.get_archive("/tmp/hello.txt")
        assert stat["name"] == "hello.txt" and stat["size"] == 7
        back = tarfile.open(fileobj=io.BytesIO(b"".join(stream)))
        assert back.extractfile("hello.txt").read() == b"payload"
        assert int(c.get_exposed_port(8080)) > 0


def test_ryuk_is_running():
    client = docker.from_env()
    names = [c.name for c in client.containers.list()]
    images = [c.image.tags for c in client.containers.list()]
    assert any("ryuk" in str(i) for i in images), (names, images)
