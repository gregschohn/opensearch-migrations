package mymodule

#PARAM_SET: [string]: #Parameters.#TemplateParameter

#S3_PARAMS: #PARAM_SET & {
	s3AwsRegion: type: string,
	s3Endpoint:  type: string,
	s3RepoUri:   type: string
}
