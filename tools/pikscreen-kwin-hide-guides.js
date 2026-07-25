function excludePikScreenGuide(window) {
    if (window.resourceClass === "pikscreen-guide") {
        window.excludeFromCapture = true;
    }
}

function watchPikScreenGuide(window) {
    excludePikScreenGuide(window);
    window.windowClassChanged.connect(() => excludePikScreenGuide(window));
}

workspace.windowAdded.connect(watchPikScreenGuide);

const windows = workspace.windowList();
for (let index = 0; index < windows.length; index++) {
    watchPikScreenGuide(windows[index]);
}
