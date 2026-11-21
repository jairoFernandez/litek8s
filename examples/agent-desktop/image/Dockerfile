# A desktop for an agent with nothing a person does not need: sway on the
# wlroots headless backend (software rendering, no X, no GPU, no session),
# Chromium as its only big piece, wayvnc for the screen and noVNC so a person
# can take over from their own browser.
FROM alpine:3.22

# sway ships with file capability cap_sys_nice, outside a container's set:
# exec would fail with EPERM. A plain copy drops it.
RUN apk add --no-cache \
        sway wayvnc chromium foot \
        novnc websockify \
        font-noto ttf-dejavu \
        grim procps tini \
    && cp /usr/bin/sway /usr/bin/sway.new && mv /usr/bin/sway.new /usr/bin/sway

COPY sway.conf /etc/agent-desktop/sway.conf
COPY start.sh /usr/local/bin/start-desktop
COPY chromium-agent chromium-close /usr/local/bin/

ENV XDG_RUNTIME_DIR=/tmp/xdg \
    WLR_BACKENDS=headless \
    WLR_RENDERER=pixman \
    WLR_LIBINPUT_NO_DEVICES=1 \
    DESKTOP_SIZE=1280x800

EXPOSE 6080 5900 9222
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/start-desktop"]
