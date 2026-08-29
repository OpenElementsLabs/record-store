# Quick Start

This walks through running Record Store locally and storing an object. Allow about
five minutes of your attention; the first container build takes longer than that on
its own, because it compiles the server from source.

## 1. Get the repository

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
```

## 2. Choose local secrets

Record Store has no built-in credentials and will not start without them. For a local
trial, put them in a `.env` file at the repository root. Git ignores it.

```bash
cat > .env <<'ENV'
RECORD_STORE_ROOT_ACCESS_KEY=trial-access-key
RECORD_STORE_ROOT_SECRET_KEY=trial-secret-key-at-least-16-chars
RECORD_STORE_CREDENTIAL_MASTER_KEY=trial-master-key-at-least-32-characters
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=trial-system-token-at-least-32-characters
ENV
```

!!! danger "These are trial values"
    They are fine on your laptop and nowhere else. Before any real deployment, read
    [Secrets and Keys](../security/encryption.md#the-credential-master-key) and generate
    real ones. The master key in particular cannot be changed later without making
    encrypted data unreadable.

## 3. Start Record Store and the console

```bash
docker compose --env-file .env -f deploy/docker/compose.console.yml up --build -d
```

The first run compiles the Rust workspace inside the image. Later runs start in
seconds.

## 4. Verify it is running

```bash
curl http://127.0.0.1:7601/health
```

```json
{"status":"ok"}
```

```bash
curl http://127.0.0.1:7601/ready
```

```json
{"status":"ready"}
```

`/health` says the process is alive. `/ready` says it can serve storage operations.
See [Health and Readiness](../operations/health-and-readiness.md).

## 5. Open the console

Go to <http://localhost:7602> and sign in with the value of
`RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN`.

The console is an administrative interface. It talks to the management API on 7601
over the Compose network — your browser never reaches 7601 directly.

## 6. Create a bucket

=== "Console"

    Open **Buckets**, choose **Create bucket**, and name it `demo`.

=== "CLI"

    The CLI reads its credential from `RECORD_STORE_MANAGEMENT_TOKEN`. Pass it into
    the container with `-e`; a shell variable on your host does not reach it.

    ```bash
    docker compose -f deploy/docker/compose.console.yml exec \
      -e RECORD_STORE_MANAGEMENT_TOKEN=trial-system-token-at-least-32-characters \
      record-store record-store bucket create demo
    ```

=== "AWS CLI"

    ```bash
    export AWS_ACCESS_KEY_ID=trial-access-key
    export AWS_SECRET_ACCESS_KEY=trial-secret-key-at-least-16-chars
    export AWS_DEFAULT_REGION=us-east-1
    export AWS_EC2_METADATA_DISABLED=true
    aws configure set s3.addressing_style path

    aws --endpoint-url http://127.0.0.1:7600 s3api create-bucket --bucket demo
    ```

## 7. Upload an object

=== "Console"

    Open the `demo` bucket and drag a file onto it.

=== "AWS CLI"

    ```bash
    echo 'hello from record store' > hello.txt
    aws --endpoint-url http://127.0.0.1:7600 s3 cp hello.txt s3://demo/hello.txt
    ```

## 8. Read it back

```bash
aws --endpoint-url http://127.0.0.1:7600 s3 cp s3://demo/hello.txt -
```

```text
hello from record store
```

## Stop it

```bash
docker compose -f deploy/docker/compose.console.yml down
```

Your data stays in the `record-store-data` volume. Add `-v` to delete it too.

## Next

- [First Bucket and Object](first-object.md) — what just happened, and what to do next
- [Application Integration](../guides/application-integration.md) — connect real code
- [Deployment](../deployment/index.md) — run this somewhere real
