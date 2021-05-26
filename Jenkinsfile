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
    booleanParam(name: 'BUILD_MACOSX',
                 description: 'Build macosx target.',
                 defaultValue: true)
    booleanParam(name: 'BUILD_DOCKER',
                 description: 'Build Docker image.',
                 defaultValue: true)
    booleanParam(name: 'BUILD_LINUX64',
                 description: 'Build x86_64-unknown-linux-gnu target.',
                 defaultValue: true)
    booleanParam(name: 'PUBLISH_ECLIPSE_DOWNLOAD',
                 description: 'Publish the resulting artifacts to Eclipse download.',
                 defaultValue: false)
    booleanParam(name: 'PUBLISH_DOCKER_HUB',
                 description: 'Publish the resulting artifacts to DockerHub.',
                 defaultValue: false)
  }
  environment {
      DOCKER_IMAGE="eclipse/zenoh-bridge-dds"
      LABEL = get_label()
      DOWNLOAD_DIR="/home/data/httpd/download.eclipse.org/zenoh/zenoh-plugin-dds/${LABEL}"
      MACOSX_DEPLOYMENT_TARGET=10.7
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

    stage('[MacMini] Build and tests') {
      when { expression { return params.BUILD_MACOSX }}
      steps {
        sh '''
        echo "Building zenoh-plugin-dds-${LABEL}"
        cargo build --release --all-targets
        cargo test --release
        '''
      }
    }

    stage('[MacMini] MacOS Package') {
      when { expression { return params.BUILD_MACOSX }}
      steps {
        sh '''
        tar -czvf zenoh-plugin-dds-${LABEL}-macosx${MACOSX_DEPLOYMENT_TARGET}-x86-64.tgz --strip-components 2 target/release/zenoh-bridge-dds
        tar -czvf zenoh-bridge-dds-${LABEL}-macosx${MACOSX_DEPLOYMENT_TARGET}-x86-64.tgz --strip-components 2 target/release/libzplugin_dds.so
        '''
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

    stage('[MacMini] x86_64-unknown-linux-gnu build') {
      when { expression { return params.BUILD_LINUX64 }}
      steps {
        sh '''
        docker run --init --rm -v $(pwd):/workdir -w /workdir adlinktech/zenoh-dev-manylinux2010-x86_64-gnu \
          /bin/bash -c "\
            rustup default ${RUST_TOOLCHAIN} && \
            cargo build --release
          "
        '''
      }
    }
    stage('[MacMini] x86_64-unknown-linux-gnu Package') {
      when { expression { return params.BUILD_LINUX64 }}
      steps {
        sh '''
        tar -czvf zenoh-plugin-dds-${LABEL}-x86_64-unknown-linux-gnu.tgz --strip-components 2 target/x86_64-unknown-linux-gnu/release/zenoh-bridge-dds
        tar -czvf zenoh-bridge-dds-${LABEL}-x86_64-unknown-linux-gnu.tgz --strip-components 2 target/x86_64-unknown-linux-gnu/release/libzplugin_dds.so
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
