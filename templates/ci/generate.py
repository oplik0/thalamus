#!/usr/bin/env python3
"""Print the target GitOps repository's credential-free rollout CI workflow."""
import argparse
import json
import re
from urllib.parse import urlsplit

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--endpoint", required=True, help="public HTTPS controller decision endpoint")
parser.add_argument("--image", required=True, help="operator-trusted Python image pinned by sha256 digest")
args = parser.parse_args()
url = urlsplit(args.endpoint)
if (url.scheme != "https" or not url.hostname or url.username or url.password
        or url.query or url.fragment or url.path != "/webhooks/github/rollout-ci/decision"):
    parser.error("endpoint must be HTTPS with path /webhooks/github/rollout-ci/decision and no credentials, query, or fragment")
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/:+-]*@sha256:[0-9a-f]{64}", args.image):
    parser.error("image must be pinned as name@sha256:<64 lowercase hex digits>")

# No checkout, repository scripts, actions, secrets, or GitHub token. The
# separate App-authored required status is the security boundary. This workflow
# verifies the controller can identify the exact event and reports its decision;
# the App status, not this job's conclusion, enforces a changing time window.
workflow = r'''name: Thalamus rollout event verification
on:
  pull_request:
    types: [opened, reopened, synchronize, edited, ready_for_review]
  merge_group:
    types: [checks_requested]
permissions: {}
jobs:
  verify-event:
    runs-on: ubuntu-latest
    timeout-minutes: 5
    container:
      image: __IMAGE__
    env:
      THALAMUS_DECISION_URL: __ENDPOINT__
      EVENT_NAME: ${{ github.event_name }}
      PR_NUMBER: ${{ github.event.number }}
      HEAD_SHA: ${{ github.event.pull_request.head.sha || github.sha }}
      GROUP_BASE_REF: ${{ github.event.merge_group.base_ref }}
      GROUP_BASE_SHA: ${{ github.event.merge_group.base_sha }}
      GROUP_HEAD_REF: ${{ github.event.merge_group.head_ref }}
    steps:
      - name: Verify event with Thalamus
        shell: sh
        run: |
          python3 - <<'PY'
          import json, os, sys, time, urllib.error, urllib.request
          event = os.environ['EVENT_NAME']
          payload = {'repository': os.environ['GITHUB_REPOSITORY'], 'event': event,
                     'sha': os.environ['HEAD_SHA'], 'number': None,
                     'base_ref': None, 'base_sha': None, 'head_ref': None}
          if event == 'pull_request':
              payload['number'] = int(os.environ['PR_NUMBER'])
          elif event == 'merge_group':
              payload.update(base_ref=os.environ['GROUP_BASE_REF'],
                             base_sha=os.environ['GROUP_BASE_SHA'],
                             head_ref=os.environ['GROUP_HEAD_REF'])
          else:
              sys.exit('unexpected event')
          data = json.dumps(payload).encode()
          request = urllib.request.Request(os.environ['THALAMUS_DECISION_URL'], data=data,
                                           headers={'Content-Type': 'application/json'}, method='POST')
          # Actions can start before the separately delivered signed webhook.
          for attempt in range(30):
              try:
                  with urllib.request.urlopen(request, timeout=15) as response:
                      result = json.load(response)
                  if result.get('verified') is not True or not isinstance(result.get('allow'), bool):
                      sys.exit('controller returned an invalid decision')
                  print('Current rollout decision:', 'allow' if result['allow'] else 'deny', '-', result.get('reason', ''))
                  print('The required GitHub App status enforces this decision and is refreshed as the window changes.')
                  sys.exit(0)
              except urllib.error.HTTPError as exc:
                  if exc.code not in (404, 503):
                      sys.exit('controller rejected event identity')
              except (urllib.error.URLError, TimeoutError):
                  pass
              if attempt != 29:
                  time.sleep(2)
          sys.exit('signed event or controller unavailable')
          PY
'''
print(workflow.replace("__IMAGE__", json.dumps(args.image)).replace("__ENDPOINT__", json.dumps(args.endpoint)), end="")
