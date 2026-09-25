# The cluster run's browser: headless Chromium, driven by Playwright.
#
# Microsoft's image carries the browsers and their system libraries and not
# the Python package, so it is added here at the same version rather than
# fetched by the pod on every run. Built by `make e2e-cluster` and handed to
# the cluster as the runtime image is; nothing pulls it from a registry.
FROM mcr.microsoft.com/playwright/python:v1.63.0-noble
RUN pip install --no-cache-dir --break-system-packages playwright==1.63.0
COPY browser.py /e2e/browser.py
ENTRYPOINT ["python", "/e2e/browser.py"]
