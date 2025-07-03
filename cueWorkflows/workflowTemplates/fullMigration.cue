package mymodule

#MIGRATION_TEMPLATES: FULL: #K8sWorkflowTemplate & {
  entrypoint:         "main"
  serviceAccountName: "argo-workflow-executor"
  parallelism: 100

  #name: "full-migration"
	#workflowParameters: #WORKFLOW_PARAMS
	_templateSignaturesMap: [string]: #TemplateSignature
	#templates: {
    main: (#WFSteps & {
      name:                        "main"
		  let s3ParamToConfigMapKeyMapping = {
  			s3AwsRegion: "AWS_REGION",
	  		s3Endpoint:  "ENDPOINT",
		  	s3RepoUri:   "repo_uri"
		  }
  	  _parsedParams: { ... }
      #parameters: {
      	sourceMigrationConfigs: type: [] | [ #SOURCE_MIGRATION_CONFIG,... ],
    	  targets:                type: [] | [ #CLUSTER_CONFIG,... ],
      	{
      		for k,v in #S3_PARAMS {
	    			"\(k)": defaultValue: #FromConfigMap & {
	  	  			#type: string,
	  		  		 map: paramWithName: #workflowParameters.s3SnapshotConfigMap,
	  			  	 key: s3ParamToConfigMapKeyMapping[k]
	  			  }
  			  }
	  		}
      }

      steps: [[
        #WorkflowStepOrTask & {
					#templateSignature:  #MIGRATION_TEMPLATES.TARGET_LATCH_HELPERS._templateSignaturesMap.init
          #argumentMappings: {
				  	prefix: argoReadyString: "workflow-{{workflow.uid}}",
  					targets: paramWithName: _parsedParams.parameterMap["targets"],
	  				configurations: paramWithName: _parsedParams.parameterMap["sourceMigrationConfigs"]
		  		}
			  }
      ]]
    })

//		  	let s3ConfigMapParameter = #workflowParameters.s3SnapshotConfigMap
//					{ toNamedParameter:  "...",  value: { map: s3ParamMap, key: "AWS_REGION", type: string } },
//					{ to: "#S3_PARAMS.s3Endpoint", map: s3ParamMap, key: "ENDPOINT" },
//					{ to: "#S3_PARAMS.s3RepoUri",    map: s3ParamMap, key: "repo_uri" }

	}
}

