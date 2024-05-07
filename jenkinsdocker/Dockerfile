FROM jenkins/jenkins:lts

USER root
RUN apt -y update
RUN apt -y install software-properties-common
RUN add-apt-repository 'deb http://deb.debian.org/debian bullseye main contrib non-free'
RUN add-apt-repository 'deb http://deb.debian.org/debian bullseye-backports main contrib non-free'
RUN apt -y install openjdk-11-jdk
RUN apt -y install gradle
RUN apt -y install docker
RUN apt -y install docker.io
RUN apt -y install npm
RUN npm install -g aws-cdk
RUN apt -y install vim less

ENV PATH=/usr/lib/jvm/java-11-openjdk-amd64/bin:$PATH
ENV JAVA_OPTS="-Djenkins.install.runSetupWizard=false"

COPY plugins.txt /usr/share/jenkins/ref/plugins.txt
RUN jenkins-plugin-cli -f /usr/share/jenkins/ref/plugins.txt

USER jenkins
RUN mkdir -p /var/jenkins_home/jobs/bootstrap/builds
#COPY configuration.yaml /var/jenkins_home/casc_configs/configuration.yaml
