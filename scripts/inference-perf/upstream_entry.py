#!/usr/bin/env python3
"""Execute the actual upstream CLI, injecting its bearer key only in memory.

0.7.0's read_config logs its merged configuration, and its report generator writes
config.yaml. Supplying server.api_key in either configuration or argv would leak
it. Injecting at client construction keeps both upstream config surfaces clean.
"""
import os
import sys
import time

import inference_perf.main as upstream


def main():
    config_file = sys.argv[1]
    api_key = os.environ.pop("THALAMUS_API_KEY")
    status_fd = int(os.environ.pop("THALAMUS_STAGE_SIGNAL_FD"))
    construct_client = upstream.vLLMModelServerClient
    run = upstream.InferencePerfRunner.run

    def authenticated_client(*args, **kwargs):
        kwargs["api_key"] = api_key
        return construct_client(*args, **kwargs)

    def timed_run(self):
        os.write(status_fd, f"start {time.monotonic_ns()}\n".encode("ascii"))
        run(self)
        os.write(status_fd, f"end {time.monotonic_ns()}\n".encode("ascii"))

    upstream.vLLMModelServerClient = authenticated_client
    upstream.InferencePerfRunner.run = timed_run
    sys.argv = ["inference-perf", "--config_file", config_file]
    upstream.main_cli()


if __name__ == "__main__":
    main()
