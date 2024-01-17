# -*- coding: utf-8 -*-

# Copied from https://developers.google.com/youtube/v3/docs/captions/insert?apix=true
# Then adapted to upload the .srt files in the current directory using the
# filename structure produced by the Rust program.

# See
#
#   https://developers.google.com/explorer-help/code-samples#python
#
# for how to get this running. You'll probably want to follow the instructions
# in config.example.toml first.

import os
from pathlib import Path

import google_auth_oauthlib.flow
import googleapiclient.discovery
from googleapiclient.errors import HttpError

from googleapiclient.http import MediaFileUpload

scopes = ["https://www.googleapis.com/auth/youtube.force-ssl"]

def main():
    api_service_name = "youtube"
    api_version = "v3"
    client_secrets_file = "client_secrets.json"

    # Get credentials and create an API client
    flow = google_auth_oauthlib.flow.InstalledAppFlow.from_client_secrets_file(client_secrets_file, scopes)
    # NOTE: modified from run_console: https://stackoverflow.com/a/77661119/472927
    credentials = flow.run_local_server(port=0)
    youtube = googleapiclient.discovery.build(api_service_name, api_version, credentials=credentials)

    try:
        for file in os.listdir('.'):
            if not os.path.isfile(file) or os.path.splitext(file)[1] != ".srt":
                continue

            video_id = file[len("YYYY-MM-DD-"):][:11]
            print("==>", video_id)

            request = youtube.captions().list(
                part="id,snippet",
                videoId=video_id
            )
            response = request.execute()
            has_gladia = False
            nstandard = 0
            for caption in response['items']:
                if not caption['snippet']['language'].startswith('en'):
                    print(" -> skipping %s language track" % caption['snippet']['language'])
                    continue
                if caption['snippet']['trackKind'] == "asr":
                    print(" -> deleting ASR track")
                    req = youtube.captions().delete(id = caption['id'])
                    res = request.execute()
                    continue
                if caption['snippet']['trackKind'] != "standard":
                    print(" -> weird caption track: %s (%s)" % (caption['snippet']['name'], caption['snippet']['trackKind']))
                    continue
                if caption['snippet']['name'] == "AI (Gladia)":
                    has_gladia = True
                    continue
                print(" -> has standard track '%s' that's not Gladia track" % caption['snippet']['name'])
    except HttpError as error:
        print(f'An HTTP error {error.resp.status} occurred: {error.content}')

if __name__ == "__main__":
    main()
