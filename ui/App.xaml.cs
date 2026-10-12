using System.Windows;

namespace WSearch.Ui;

public partial class App : Application
{
    protected override void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        Theme.Apply(this);
        Theme.Watch(this);
    }
}
