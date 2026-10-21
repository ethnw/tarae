# compat's Java image (java-gradle, java-maven): Debian's JDK 21 and Maven, and Gradle from services.gradle.org,
# its sha256 pinned here (the version macOS's runs had). The wrapper's own distribution and the dependencies are
# the pre-warm's, in ~/.gradle and ~/.m2 on the stack's volume.
# Built by `cargo xtask compat` on rust:<pin> (BASE); the tag is the recipe's hash.
ARG BASE
FROM ${BASE}
ARG GRADLE=9.8.1
RUN set -eu; \
    apt-get update -qq; \
    apt-get install -y -qq --no-install-recommends openjdk-21-jdk-headless maven unzip >/dev/null; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSLo /tmp/gradle.zip "https://services.gradle.org/distributions/gradle-$GRADLE-bin.zip"; \
    echo "dce76f55f8e251a3a1f130eb120f30b3d271de2b76c9b0729d316b5a1b6dc01f  /tmp/gradle.zip" | sha256sum -c -; \
    unzip -q /tmp/gradle.zip -d /opt; \
    ln -s "/opt/gradle-$GRADLE/bin/gradle" /usr/local/bin/gradle; \
    rm /tmp/gradle.zip; \
    java -version; mvn -v | head -1; \
    GRADLE_USER_HOME=/tmp/gradle-home gradle --version | grep '^Gradle'; rm -rf /tmp/gradle-home
