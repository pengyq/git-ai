using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.ComponentModel.Composition;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
using GitAiVS.Detection;
using GitAiVS.Services;
using Microsoft.VisualStudio.Editor;
using Microsoft.VisualStudio.Text;
using Microsoft.VisualStudio.Text.Editor;
using Microsoft.VisualStudio.TextManager.Interop;
using Microsoft.VisualStudio.Utilities;

namespace GitAiVS.Listeners
{
    /// <summary>
    /// Tracks which files were recently touched by an AI agent.
    /// </summary>
    internal sealed class TrackedAgent
    {
        public const long StaleThresholdMs = 300_000;
        public const long RefreshEligibilityWindowMs = 15_000;

        public string AgentName { get; }
        public string WorkspaceRoot { get; }
        public string LastCheckpointContent { get; }
        public long TrackedAt { get; }

        public TrackedAgent(string agentName, string workspaceRoot, string lastCheckpointContent, long trackedAt)
        {
            AgentName = agentName;
            WorkspaceRoot = workspaceRoot;
            LastCheckpointContent = lastCheckpointContent;
            TrackedAt = trackedAt;
        }
    }

    /// <summary>
    /// Attaches to every opened text editor in Visual Studio and listens for
    /// ITextBuffer.Changed events. Uses stack trace analysis to detect AI edits
    /// and dispatches checkpoints accordingly.
    /// </summary>
    [Export(typeof(IVsTextViewCreationListener))]
    [ContentType("text")]
    [TextViewRole(PredefinedTextViewRoles.Editable)]
    public sealed class TextBufferListener : IVsTextViewCreationListener
    {
        [Import]
        internal IVsEditorAdaptersFactoryService? AdapterService { get; set; }

        private readonly ConcurrentDictionary<string, TrackedAgent> _agentTouchedFiles = new();
        private readonly ConcurrentDictionary<string, CancellationTokenSource> _pendingCheckpoints = new();
        private readonly ConcurrentDictionary<string, string> _fileContentBeforeEdit = new();
        private readonly ConcurrentDictionary<string, long> _beforeEditTriggered = new();
        private readonly ConcurrentDictionary<ITextBuffer, TabCompletionFilter> _tabFilters = new();

        private const int DebounceMs = 300;
        private const long BeforeEditExpiryMs = 5000;

        internal CheckpointService? CheckpointSvc { get; set; }

        public void VsTextViewCreated(IVsTextView textViewAdapter)
        {
            if (AdapterService == null) return;

            var textView = AdapterService.GetWpfTextView(textViewAdapter);
            if (textView == null) return;

            var buffer = textView.TextBuffer;

            var tabFilter = new TabCompletionFilter();
            tabFilter.AttachToView(textViewAdapter);
            _tabFilters[buffer] = tabFilter;

            buffer.Changed += OnBufferChanged;

            textView.Closed += (_, __) =>
            {
                buffer.Changed -= OnBufferChanged;
                _tabFilters.TryRemove(buffer, out TabCompletionFilter _);
            };
        }

        private void OnBufferChanged(object sender, TextContentChangedEventArgs e)
        {
            if (CheckpointSvc == null) return;

            var buffer = sender as ITextBuffer;
            if (buffer == null) return;

            var filePath = GetFilePath(buffer);
            if (filePath == null) return;

            var stackTrace = new StackTrace();
            var analysis = CopilotEditDetector.Analyze(stackTrace);

            LogBufferChange(filePath, e, analysis);

            if (analysis.Confidence != Confidence.High || analysis.AgentName == null)
                return;

            var workspaceRoot = GitRepoResolver.FindRepoRoot(filePath);
            if (workspaceRoot == null) return;

            var now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

            SendBeforeEditIfNeeded(filePath, workspaceRoot, analysis.AgentName, buffer, now);

            UpdateTracking(filePath, workspaceRoot, analysis.AgentName, buffer.CurrentSnapshot.GetText(), now);

            ScheduleAfterEditCheckpoint(filePath, workspaceRoot, analysis.AgentName, buffer);
        }

        private void SendBeforeEditIfNeeded(string filePath, string workspaceRoot, string agentName, ITextBuffer buffer, long now)
        {
            if (_beforeEditTriggered.TryGetValue(filePath, out var lastTriggered) && (now - lastTriggered) < BeforeEditExpiryMs)
                return;

            var currentContent = buffer.CurrentSnapshot.GetText();
            _fileContentBeforeEdit[filePath] = currentContent;
            _beforeEditTriggered[filePath] = now;

            var relativePath = GitRepoResolver.ToRelativePath(filePath, workspaceRoot);

            var dirtyFiles = new Dictionary<string, string> { { relativePath, currentContent } };

            Debug.WriteLine($"[git-ai] Triggering human checkpoint (before edit by {agentName}) on {relativePath}");

            _ = Task.Run(() => CheckpointSvc!.SendBeforeEditAsync(
                workspaceRoot,
                new[] { relativePath },
                dirtyFiles));
        }

        private void UpdateTracking(string filePath, string workspaceRoot, string agentName, string content, long now)
        {
            _agentTouchedFiles[filePath] = new TrackedAgent(agentName, workspaceRoot, content, now);
        }

        private void ScheduleAfterEditCheckpoint(string filePath, string workspaceRoot, string agentName, ITextBuffer buffer)
        {
            if (_pendingCheckpoints.TryRemove(filePath, out var existingCts))
                existingCts.Cancel();

            var cts = new CancellationTokenSource();
            _pendingCheckpoints[filePath] = cts;

            _ = Task.Delay(DebounceMs, cts.Token).ContinueWith(t =>
            {
                if (t.IsCanceled) return;

                _pendingCheckpoints.TryRemove(filePath, out CancellationTokenSource _);
                _fileContentBeforeEdit.TryRemove(filePath, out string _);
                _beforeEditTriggered.TryRemove(filePath, out long _);

                var contentAfterEdit = buffer.CurrentSnapshot.GetText();
                var relativePath = GitRepoResolver.ToRelativePath(filePath, workspaceRoot);

                var dirtyFiles = new Dictionary<string, string> { { relativePath, contentAfterEdit } };

                Debug.WriteLine($"[git-ai] Triggering ai_agent checkpoint for {agentName} on {relativePath}");

                _ = CheckpointSvc?.SendAfterEditAsync(
                    workspaceRoot,
                    new[] { relativePath },
                    agentName,
                    dirtyFiles);
            }, TaskScheduler.Default);
        }

        private static string? GetFilePath(ITextBuffer buffer)
        {
            if (buffer.Properties.TryGetProperty(typeof(ITextDocument), out ITextDocument document))
                return document.FilePath;

            return null;
        }

        private static void LogBufferChange(string filePath, TextContentChangedEventArgs e, AnalysisResult analysis)
        {
            if (analysis.AgentName != null)
            {
                Debug.WriteLine($"[git-ai] Buffer change detected on {Path.GetFileName(filePath)}");
                Debug.WriteLine($"[git-ai]   Source: {analysis.AgentName} (confidence: {analysis.Confidence})");
                Debug.WriteLine($"[git-ai]   Relevant frames:\n{CopilotEditDetector.FormatRelevantFrames(analysis.RelevantFrames)}");
            }
        }
    }
}
