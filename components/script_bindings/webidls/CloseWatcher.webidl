// https://html.spec.whatwg.org/multipage/interaction.html#the-closewatcher-interface

[Exposed=Window]
interface CloseWatcher : EventTarget {
  [Throws] constructor(optional CloseWatcherOptions options = {});

  undefined requestClose();
  undefined close();
  undefined destroy();

  attribute EventHandler oncancel;
  attribute EventHandler onclose;
};

dictionary CloseWatcherOptions {
  // AbortSignal signal;
};