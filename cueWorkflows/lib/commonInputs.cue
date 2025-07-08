package mymodule

#TEMPLATE_PARAM_SET: [string]: #TemplateParameterDefinition
#WORKFLOW_PARAM_SET: [string]: #WorkflowParameterDefinition

#S3_PARAMS: #TEMPLATE_PARAM_SET & {
	s3AwsRegion: type: string,
	s3Endpoint:  type: string,
	s3RepoUri:   type: string
}

#WORKFLOW_PARAMS: #WORKFLOW_PARAM_SET & {
	etcdEndpoints:       parameterValue: "http://etcd.ma.svc.cluster.local:2379"
	etcdUser:            parameterValue: "root"
	etcdPassword:        parameterValue: "password"
	etcdImage:           parameterValue: "migrations/migration_console:latest"
	s3SnapshotConfigMap: parameterValue: "s3-snapshot-config"
}
