import * as cdk from 'aws-cdk-lib';
import * as s3 from 'aws-cdk-lib/aws-s3';
import * as iam from 'aws-cdk-lib/aws-iam';
import { Construct } from 'constructs';

export class InfraStack extends cdk.Stack {
  public readonly bucket: s3.Bucket;
  public readonly uploadRole: iam.Role;
  public readonly backupUser: iam.User;

  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // S3 bucket for Google Photos backup.
    // All objects are immediately transitioned to Glacier Deep Archive.
    this.bucket = new s3.Bucket(this, 'PhotoBackupBucket', {
      bucketName: 'google-photos-backup-p3n8wd5z1fyc',
      encryption: s3.BucketEncryption.S3_MANAGED,
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      enforceSSL: true,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
      versioned: true,
      lifecycleRules: [
        {
          id: 'ImmediateDeepArchive',
          enabled: true,
          transitions: [
            {
              storageClass: s3.StorageClass.DEEP_ARCHIVE,
              transitionAfter: cdk.Duration.days(0),
            },
          ],
        },
      ],
    });

    // IAM user whose long-term credentials are used by the backup script.
    // After deploying, create an access key for this user in the AWS console
    // and set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY in your .env file.
    this.backupUser = new iam.User(this, 'PhotoBackupUser', {
      userName: 'google-photos-backup-user',
    });

    // IAM role with the actual S3 write permission.
    // Only the backup user above is trusted to assume it.
    this.uploadRole = new iam.Role(this, 'PhotoUploadRole', {
      assumedBy: new iam.ArnPrincipal(this.backupUser.userArn),
      description: 'Allows uploading objects to the photo backup bucket',
    });

    this.uploadRole.addToPolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['s3:PutObject'],
        resources: [`${this.bucket.bucketArn}/*`],
      }),
    );

    // Allow the backup user to assume the upload role.
    this.backupUser.addToPolicy(
      new iam.PolicyStatement({
        effect: iam.Effect.ALLOW,
        actions: ['sts:AssumeRole'],
        resources: [this.uploadRole.roleArn],
      }),
    );

    new cdk.CfnOutput(this, 'BucketName', { value: this.bucket.bucketName });
    new cdk.CfnOutput(this, 'UploadRoleArn', { value: this.uploadRole.roleArn });
    new cdk.CfnOutput(this, 'BackupUserName', { value: this.backupUser.userName });
  }
}
