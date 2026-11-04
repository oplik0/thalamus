FROM quay.io/inference-perf/inference-perf@sha256:d247e6ad725dc5bb56df71d115abc8ca455bba9f0ccd4110e14bbb56504250ec

COPY scripts/inference-perf/adapter.py scripts/inference-perf/snapshot.py scripts/inference-perf/upstream_entry.py /opt/thalamus-inference-perf/
ENV HOME=/tmp HF_HOME=/tmp TMPDIR=/tmp PYTHONDONTWRITEBYTECODE=1
USER 65532:65532
ENTRYPOINT ["python", "/opt/thalamus-inference-perf/adapter.py"]
CMD []
