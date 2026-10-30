# Container image for building sttme Inno Setup installer on Linux without Windows.
# Usage:
#   docker build -t sttme-innosetup packaging/windows
#   docker run --rm -v "$PWD:/work" sttme-innosetup packaging/windows/sttme.iss
FROM amake/innosetup:latest

LABEL org.opencontainers.image.title="sttme Inno Setup Builder"
LABEL org.opencontainers.image.description="Docker container to compile sttme Windows installers on Linux"
LABEL org.opencontainers.image.source="https://github.com/maxfridbe/sttme"

WORKDIR /work

ENTRYPOINT ["iscc"]
