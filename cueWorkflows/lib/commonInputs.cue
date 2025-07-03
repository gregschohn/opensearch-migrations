package mymodule

#TEMPLATE_PARAM_SET: [string]: #TemplateParameterDefinition
#WORKFLOW_PARAM_SET: [string]: #WorkflowParameterDefinition

#S3_PARAMS: #TEMPLATE_PARAM_SET & {
//	s3AwsRegion: type: string,
//	s3Endpoint:  type: string,
//	s3RepoUri:   type: string
}

#WORKFLOW_PARAMS: #WORKFLOW_PARAM_SET & {
	etcdEndpoints:       defaultValue: "http://etcd.ma.svc.cluster.local:2379"
	etcdUser:            defaultValue: "root"
	etcdPassword:        defaultValue: "password"
	etcdImage:           defaultValue: "migrations/migration_console:latest"
	s3SnapshotConfigMap: defaultValue: "s3-snapshot-config"
}
