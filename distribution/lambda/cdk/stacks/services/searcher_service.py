import aws_cdk
from aws_cdk import aws_lambda, aws_s3, PhysicalName
from constructs import Construct


class SearcherService(Construct):
    def __init__(
        self,
        scope: Construct,
        construct_id: str,
        store_bucket: aws_s3.Bucket,
        index_id: str,
        memory_size: int,
        environment: dict[str, str],
        asset_path: str,
        **kwargs
    ) -> None:
        super().__init__(scope, construct_id, **kwargs)

        self.lambda_function = aws_lambda.Function(
            self,
            id="Lambda",
            code=aws_lambda.Code.from_asset(asset_path),
            runtime=aws_lambda.Runtime.PROVIDED_AL2,
            handler="N/A",
            environment={
                "QW_LAMBDA_INDEX_BUCKET": store_bucket.bucket_name,
                "QW_LAMBDA_METASTORE_URI": f"s3://${store_bucket.bucket_name}/index#polling_interval=60s",
                "QW_LAMBDA_INDEX_ID": index_id,
                **environment,
            },
            timeout=aws_cdk.Duration.seconds(30),
            memory_size=memory_size,
            ephemeral_storage_size=aws_cdk.Size.gibibytes(10),
        )

        store_bucket.grant_read_write(self.lambda_function)
