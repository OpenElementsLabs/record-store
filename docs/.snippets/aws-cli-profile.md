??? note "Set up the `record-store` AWS CLI profile"
    The AWS CLI commands on this page use a profile named `record-store`. The
    profile sends every request to Record Store, never to AWS. If you haven't
    created the profile yet, run the following commands once:

    ```bash
    aws configure set endpoint_url ENDPOINT --profile record-store
    aws configure set aws_access_key_id ACCESS_KEY --profile record-store
    aws configure set aws_secret_access_key SECRET_KEY --profile record-store
    aws configure set region us-east-1 --profile record-store
    aws configure set s3.addressing_style path --profile record-store
    aws configure set request_checksum_calculation when_required --profile record-store
    aws configure set response_checksum_validation when_required --profile record-store
    ```

  Replace the following:

  - `ENDPOINT`: the URL of the Record Store S3 API, which listens on port 7600
      by default. For example, `https://storage.example.com` or
      `http://127.0.0.1:7600`. Don't use the management API (port 7601) or the
      web console (port 7602).

  - `ACCESS_KEY` and `SECRET_KEY`: S3 credentials. Use a
      [service account](../administration/service-accounts.md)'s access key
      pair, or the root key pair for local testing. The management token
      doesn't work with the S3 API.

    For an explanation of each setting, see [AWS CLI](../guides/aws-cli.md).
