# Quick start

In this quick start, you run Record Store and its web console from the published
container images, and then you store an object and read it back. You don't
compile anything or clone the repository. This quick start takes about five
minutes.

To build Record Store from source instead, see
[Development setup](../contributing/development-setup.md).

## Before you begin

Install the following tools. To check that a tool is installed, run the command
in the **Check** column.

| Tool | Check | Purpose |
| --- | --- | --- |
| Docker with the Compose v2 plugin | `docker compose version` | Runs Record Store. |
| curl | `curl --version` | Downloads the Compose file and checks health. |
| [AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) 2.13 or later (optional) | `aws --version` | Sends requests to the S3 API. |

You can complete this quick start in the web console alone. Install the AWS CLI
if you also want to send requests to the S3 API, which is how applications
connect to Record Store. If you don't install it, skip
[Configure the AWS CLI](#configure-the-aws-cli) and use the **Console** tabs.

> This quick start was tested with Record Store 0.2.1 and AWS CLI 2.27.

## Download the Compose file

Create a directory for this quick start and download the Compose file for the
release:

```bash
mkdir record-store-quickstart && cd record-store-quickstart
curl -fsSL -o compose.yaml \
  https://raw.githubusercontent.com/OpenElementsLabs/record-store/v0.2.1/deploy/docker/compose.ghcr.yml
```

The Compose file runs two public images, `ghcr.io/openelementslabs/record-store`
and `ghcr.io/openelementslabs/record-store-console`, so you don't need to run
`docker login`. 

Record Store stores your data in a Docker volume named
`record-store-data`.

## Set the credentials

Record Store has no built-in credentials and doesn't start until you provide
them. Compose reads credentials from a `.env` file in the same directory as
`compose.yaml`.

To create the `.env` file and make it readable only by you, run the following
commands:

```bash
cat > .env <<'ENV'
RECORD_STORE_VERSION=0.2.1
RECORD_STORE_ROOT_ACCESS_KEY=trial-access-key
RECORD_STORE_ROOT_SECRET_KEY=trial-secret-key-at-least-16-chars
RECORD_STORE_CREDENTIAL_MASTER_KEY=trial-master-key-at-least-32-characters
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=trial-system-token-at-least-32-characters
ENV
chmod 600 .env
```

The file defines credentials for two different APIs, plus a key that Record
Store uses internally:

| Variable | Description | Used by |
| --- | --- | --- |
| `RECORD_STORE_ROOT_ACCESS_KEY`, `RECORD_STORE_ROOT_SECRET_KEY` | **S3 credentials**: an access key pair, like an AWS access key pair. | The AWS CLI and SDKs, on port 7600. |
| `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` | **Management token**: a bearer token for administration. | The console and the `record-store` CLI, on port 7601. |
| `RECORD_STORE_CREDENTIAL_MASTER_KEY` | Encrypts stored credentials. | Record Store only. You never send it to any API. |

`RECORD_STORE_VERSION` sets both images to the same release.

!!! danger "Use these values only on your own computer"
    These values are for this quick start only. Before you deploy Record Store,
    generate real secrets as described in
    [Secrets and keys](../security/encryption.md#the-credential-master-key).
    You can't change the master key later without making encrypted data
    unreadable.

## Start Record Store and the console

To start the containers in the background, run the following command:

```bash
docker compose up -d
```

On the first run, Docker downloads the images. Compose starts the console after
Record Store reports that it is healthy.

## Check that Record Store is running

1.  Check that the process is running:

    ```bash
    curl http://127.0.0.1:7601/health
    ```

    The output is the following:

    ```json
    {"status":"ok"}
    ```

1.  Check that Record Store is ready to serve storage requests:

    ```bash
    curl http://127.0.0.1:7601/ready
    ```

    The output is the following:

    ```json
    {"status":"ready"}
    ```

For more information about these endpoints, see
[Health and readiness](../operations/health-and-readiness.md).

## Sign in to the console

1.  In your browser, go to <http://localhost:7602>.
2.  Enter the management token from your `.env` file:
    `trial-system-token-at-least-32-characters`.

You use the console's **Buckets** and **Objects** pages later in this quick
start.

### If the sign-in page appears again

If the console returns you to the sign-in page after you enter the token, your
browser rejected the console's session cookie. The console marks this cookie as
`Secure`, and some browsers don't accept secure cookies over
`http://localhost`.

To let the console use the cookie over HTTP, do the following:

1.  Add the following line to your `.env` file:

    ```bash
    RECORD_STORE_CONSOLE_SECURE_COOKIES=false
    ```

1.  Apply the change:

    ```bash
    docker compose up -d
    ```

1.  Sign in again.

!!! warning "Use this setting only for local testing"
    Remove `RECORD_STORE_CONSOLE_SECURE_COOKIES=false` before you serve the
    console over TLS.

## Configure the AWS CLI

The AWS CLI works with any S3-compatible service, not only Amazon S3. In this
quick start, the AWS CLI sends every request to your local Record Store. Nothing
is sent to AWS, and you don't need an AWS account.

To keep your existing AWS configuration unchanged, create a separate *profile*.
A profile is a named set of AWS CLI credentials and settings. Set every value in
this section before you send your first request.

1.  Create a profile named `record-store` that points to your local Record
    Store:

    ```bash
    aws configure set aws_access_key_id trial-access-key --profile record-store
    aws configure set aws_secret_access_key trial-secret-key-at-least-16-chars --profile record-store
    aws configure set region us-east-1 --profile record-store
    aws configure set endpoint_url http://127.0.0.1:7600 --profile record-store
    aws configure set s3.addressing_style path --profile record-store
    ```

1.  Set the environment variables for your current shell:

    ```bash
    export AWS_PROFILE=record-store
    export AWS_EC2_METADATA_DISABLED=true
    export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
    export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
    ```

1.  Confirm that the AWS CLI uses the `record-store` profile:

    ```bash
    aws configure list
    ```

    In the output, the `profile` row shows `record-store`.

!!! warning "Run the `export` commands in every new terminal"
    Environment variables apply only to the current shell. If you open a new
    terminal and don't set `AWS_PROFILE=record-store`, the AWS CLI uses your
    default profile. If your default profile has real AWS credentials, the
    commands in this quick start go to your AWS account instead of to Record
    Store.

The following table explains each setting:

| Setting | Reason |
| --- | --- |
| Access key ID and secret access key | The root **S3 credentials** from `.env`. These credentials exist only in your Record Store, not in AWS. Don't use the management token here. |
| `region us-east-1` | Record Store has no regions, but SigV4 signing requires one. Any consistent value works. |
| `endpoint_url` | Sends requests from this profile to Record Store instead of AWS. |
| `s3.addressing_style path` | Puts the bucket name in the URL path, for example `127.0.0.1:7600/demo`, instead of in the hostname. |
| `AWS_PROFILE` | Makes the AWS CLI use the `record-store` profile in the current shell. |
| `AWS_EC2_METADATA_DISABLED` | Prevents a delay while the CLI looks for EC2 instance metadata. |
| `AWS_REQUEST_CHECKSUM_CALCULATION`, `AWS_RESPONSE_CHECKSUM_VALIDATION` | Prevents the CLI from using `aws-chunked` trailing-checksum encoding, which Record Store doesn't support. Without these variables, uploads from recent CLI versions fail with a `NotImplemented` error. |

!!! note "Use root credentials only for testing"
    Applications must use a
    [service account](../administration/service-accounts.md) with only the
    permissions that they need. For more information, see
    [Application integration](../guides/application-integration.md).

## Create a bucket

Create a bucket named `demo` by using one of the following methods:

 ### AWS CLI

Run the following command:

```bash
aws s3api create-bucket --bucket demo
```

The output is the following:

```json
{
    "Location": "/demo"
}
```

### Console

1.  Click **Buckets**.
2.  Click **Create bucket**.
3.  Enter `demo` as the name.

### Record Store CLI

The `record-store` CLI in the container authenticates with the **management token**. Shell variables on your host don't reach the container, so pass the token with the `-e` flag:

```bash
docker compose exec \
    -e RECORD_STORE_MANAGEMENT_TOKEN=trial-system-token-at-least-32-characters \
    record-store record-store bucket create demo
```


## Upload and download an object

Upload an object to the `demo` bucket and download it again by using one of the
following methods:

### AWS CLI

To upload a file, download it again, and compare the two copies, run the following commands:

```bash
echo 'hello from record store' > hello.txt
aws s3 cp hello.txt s3://demo/hello.txt
aws s3 cp s3://demo/hello.txt downloaded.txt
cmp hello.txt downloaded.txt && echo "contents match"
```

The output is the following:

```text
upload: ./hello.txt to s3://demo/hello.txt
download: s3://demo/hello.txt to ./downloaded.txt
contents match

```

The object also appears in the console, in the `demo` bucket.

If the upload fails with a `NotImplemented` error, the checksum variables aren't set in your current shell. Run the `export` commands from [Configure the AWS CLI](#configure-the-aws-cli) again.

### Console

1. Create a small text file on your computer, for example `hello.txt`.
2. Click **Objects**, and then open the `demo` bucket.
3. Upload `hello.txt`.
4. Select `hello.txt` to preview it, and then download it.

For more information, see [Web console](../guides/web-console.md).

## Clean up

To stop the containers and keep your data in the `record-store-data` volume, run
the following command:

```bash
docker compose down
```

To start Record Store again with your existing data, run the following command
from the same directory:

```bash
docker compose up -d
```

!!! danger "Delete the data only when you're finished"
    The `docker compose down -v` command also deletes the `record-store-data`
    volume, including every bucket and object. Run it only when you no longer
    need the data.

## What's next

- To learn about buckets, object keys, and service accounts, see
  [Buckets, objects, and service accounts](first-object.md).
- To connect an application, see
  [Application integration](../guides/application-integration.md).
- To deploy Record Store for real use, see [Deployment](../deployment/index.md).
