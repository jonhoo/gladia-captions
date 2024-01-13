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

import google_auth_oauthlib.flow
import googleapiclient.discovery
import googleapiclient.errors

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

    for file in os.listdir('.'):
        if not os.path.isfile(file) or os.path.splitext(file)[1] != ".srt":
            continue

        video_id = file[len("YYYY-MM-DD-"):][:11]
        print(video_id)

        request = youtube.captions().insert(
            part="snippet",
            body=dict(
              snippet=dict(
                videoId=video_id,
                language="en-US",
                name="English",
                isDraft=False
              )
            ),
            media_body=MediaFileUpload(file)
        )
        response = request.execute()

if __name__ == "__main__":
    main()
