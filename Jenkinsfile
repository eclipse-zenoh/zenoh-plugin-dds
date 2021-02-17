pipeline {
  agent { label 'MacMini' }
  options { skipDefaultCheckout() }
  parameters {
    gitParameter(name: 'GIT_TAG',
                 type: 'PT_BRANCH_TAG',
                 description: 'The Git tag to checkout. If not specified "master" will be checkout.',
                 defaultValue: 'master')
    string(name: 'DOCKER_TAG',
           description: 'An extra Docker tag (e.g. "latest"). By default GIT_TAG will also be used as Docker tag',
           defaultValue: '')
    booleanParam(name: 'PUBLISH_DOCKER_HUB',
                 description: 'Publish the resulting artifacts to DockerHub.',
                 defaultValue: false)
  }
  environment {
      DOCKER_IMAGE="eclipse/zenoh-bridge-dds"
      LABEL = get_label()
  }

  stages {

    stage('[MacMini] Checkout Git TAG') {
      steps {
        deleteDir()
        checkout([$class: 'GitSCM',
                  branches: [[name: "${params.GIT_TAG}"]],
                  doGenerateSubmoduleConfigurations: false,
                  extensions: [],
                  gitTool: 'Default',
                  submoduleCfg: [],
                  userRemoteConfigs: [[url: 'https://github.com/eclipse-zenoh/zenoh-plugin-dds.git']]
                ])
      }
    }

    stage('[MacMini] Docker build') {
      steps {
        sh '''
        if [ -n "${DOCKER_TAG}" ]; then
          export EXTRA_TAG="-t ${DOCKER_IMAGE}:${DOCKER_TAG}"
        fi
        docker build -t ${DOCKER_IMAGE}:${LABEL} ${EXTRA_TAG} .
        '''
      }
    }

    stage('[MacMini] Publish to Docker Hub') {
      when { expression { return params.PUBLISH_DOCKER_HUB }}
      steps {
        withCredentials([usernamePassword(credentialsId: 'dockerhub-bot',
            passwordVariable: 'DOCKER_HUB_CREDS_PSW', usernameVariable: 'DOCKER_HUB_CREDS_USR')])
        {
          sh '''
          docker login -u ${DOCKER_HUB_CREDS_USR} -p ${DOCKER_HUB_CREDS_PSW}
          docker push ${DOCKER_IMAGE}:${LABEL}
          if [ -n "${DOCKER_TAG}" ]; then
            docker push ${DOCKER_IMAGE}:${DOCKER_TAG}
          fi
          docker logout
          '''
        }
      }
    }
  }
}

def get_label() {
    return env.GIT_TAG.startsWith('origin/') ? env.GIT_TAG.minus('origin/') : env.GIT_TAG
}
