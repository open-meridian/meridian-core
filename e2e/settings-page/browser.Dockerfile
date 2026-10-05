# The settings pages' browser: headless Chromium, driven by Playwright, as
# the cluster run's (e2e/cluster/browser.Dockerfile), at the same version.
FROM mcr.microsoft.com/playwright/python:v1.63.0-noble
RUN pip install --no-cache-dir --break-system-packages playwright==1.63.0
COPY browser.py /e2e/browser.py
ENTRYPOINT ["python", "/e2e/browser.py"]
